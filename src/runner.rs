// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use prettydiff::basic::DiffOp;
use prettydiff::diff_lines;
use tokio::fs::{read_dir, remove_file, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;
use walkdir::WalkDir;

use crate::case::TestCase;
use crate::error::{Result, SqlnessError};
use crate::{config::Config, environment::EnvController};

/// The entrypoint of this crate.
///
/// To run your integration test cases, simply [`new`] a `Runner` and [`run`] it.
///
/// [`new`]: crate::Runner#method.new
/// [`run`]: crate::Runner#method.run
///
/// ```rust, ignore, no_run
/// async fn run_integration_test() {
///     let runner = Runner::new(root_path, env).await;
///     runner.run().await;
/// }
/// ```
///
/// For more detailed explaination, refer to crate level documentment.
pub struct Runner<E: EnvController> {
    config: Config,
    env_controller: Arc<E>,
}

impl<E: EnvController> Runner<E> {
    pub async fn try_new<P: AsRef<Path>>(config_path: P, env: E) -> Result<Self> {
        let mut config_file =
            File::open(config_path.as_ref())
                .await
                .map_err(|e| SqlnessError::ReadPath {
                    source: e,
                    path: config_path.as_ref().to_path_buf(),
                })?;

        let mut config_buf = vec![];
        config_file.read_to_end(&mut config_buf).await?;
        let config: Config =
            toml::from_slice(&config_buf).map_err(|e| SqlnessError::ParseToml {
                source: e,
                file: config_path.as_ref().to_path_buf(),
            })?;

        Ok(Self {
            config,
            env_controller: Arc::new(env),
        })
    }

    pub async fn new_with_config(config: Config, env: E) -> Result<Self> {
        Ok(Self {
            config,
            env_controller: Arc::new(env),
        })
    }

    pub async fn run(&self) -> Result<()> {
        let environments = self.collect_env().await?;
        for env in environments {
            let env_config = self.read_env_config(&env).await;
            let config_path = env_config.as_path();
            let config_path = if config_path.exists() {
                Some(config_path)
            } else {
                None
            };
            let db = self.env_controller.start(&env, config_path).await;
            if let Err(e) = self.run_env(&env, &db).await {
                println!("Environment {} run failed with error {:?}", env, e);
            }
            self.env_controller.stop(&env, db).await;
        }

        Ok(())
    }

    async fn read_env_config(&self, env: &str) -> PathBuf {
        let mut path_buf = std::path::PathBuf::new();
        path_buf.push(&self.config.case_dir);
        path_buf.push(env);
        path_buf.push(&self.config.env_config_file);

        path_buf
    }

    async fn collect_env(&self) -> Result<Vec<String>> {
        let mut dirs = read_dir(&self.config.case_dir).await?;
        let mut result = vec![];

        while let Some(dir) = dirs.next_entry().await? {
            if dir.file_type().await?.is_dir() {
                let file_name = dir.file_name().to_str().unwrap().to_string();
                result.push(file_name);
            }
        }

        Ok(result)
    }

    async fn run_env(&self, env: &str, db: &E::DB) -> Result<()> {
        let case_paths = self.collect_case_paths(env).await?;
        let mut diff_cases = vec![];
        let mut errors = vec![];
        let start = Instant::now();
        for path in case_paths {
            let case_result = self.run_single_case(db, &path).await;
            let case_name = path.as_os_str().to_str().unwrap().to_owned();
            match case_result {
                Ok(true) => diff_cases.push(case_name),
                Ok(false) => {}
                Err(e) => {
                    if self.config.fail_fast {
                        println!("Case {} failed with error {:?}", case_name, e);
                        println!("Stopping environment {} due to previous error.", env);
                        break;
                    } else {
                        errors.push((case_name, e))
                    }
                }
            }
        }

        println!(
            "Environment {} run finished, cost:{}ms",
            env,
            start.elapsed().as_millis()
        );

        let mut error_count = 0;
        if !diff_cases.is_empty() {
            println!("Different cases:");
            println!("{:#?}", diff_cases);
            error_count += diff_cases.len();
        }
        if !errors.is_empty() {
            println!("Error cases:");
            println!("{:#?}", errors);
            error_count += errors.len();
        }
        if error_count == 0 {
            Ok(())
        } else {
            Err(SqlnessError::RunFailed { count: error_count })
        }
    }

    async fn run_single_case(&self, db: &E::DB, path: &PathBuf) -> Result<bool> {
        let case_path = path.with_extension(&self.config.test_case_extension);
        let case = TestCase::from_file(case_path, &self.config).await?;
        let output_path = path.with_extension(&self.config.output_result_extension);
        let mut output_file = Self::open_output_file(&output_path).await?;

        let timer = Instant::now();
        case.execute(db, &mut output_file).await?;
        let elapsed = timer.elapsed();

        output_file.flush().await?;
        let is_different = self.compare(&path).await?;
        if !is_different {
            remove_file(output_path).await?;
        }

        println!(
            "Test case {:?} finished, cost: {}ms",
            path.as_os_str(),
            elapsed.as_millis()
        );
        Ok(is_different)
    }

    async fn collect_case_paths(&self, env: &str) -> Result<Vec<PathBuf>> {
        let mut root = PathBuf::from_str(&self.config.case_dir).unwrap();
        root.push(env);

        let test_case_extension = self.config.test_case_extension.as_str();
        let mut cases: Vec<_> = WalkDir::new(&root)
            .into_iter()
            .filter_map(|entry| {
                entry
                    .map_or(None, |entry| Some(entry.path().to_path_buf()))
                    .filter(|path| {
                        path.extension()
                            .map(|ext| ext == test_case_extension)
                            .unwrap_or(false)
                    })
            })
            .map(|path| path.with_extension(""))
            .filter(|path| {
                path.file_name()
                    .unwrap_or_default()
                    .to_str()
                    .unwrap_or_default()
                    .contains(&self.config.test_filter)
            })
            .collect();

        // sort the cases in an os-independent order.
        cases.sort_by(|a, b| {
            let a_lower = a.to_string_lossy().to_lowercase();
            let b_lower = b.to_string_lossy().to_lowercase();
            a_lower.cmp(&b_lower)
        });

        Ok(cases)
    }

    async fn open_output_file<P: AsRef<Path>>(path: P) -> Result<File> {
        Ok(OpenOptions::default()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .await?)
    }

    /// Compare files' diff, return true if two files are different
    async fn compare<P: AsRef<Path>>(&self, path: P) -> Result<bool> {
        let mut result_lines = vec![];
        File::open(
            path.as_ref()
                .with_extension(&self.config.expect_result_extension),
        )
        .await?
        .read_to_end(&mut result_lines)
        .await?;
        let result_lines = String::from_utf8(result_lines)?;

        let mut output_lines = vec![];
        File::open(
            path.as_ref()
                .with_extension(&self.config.output_result_extension),
        )
        .await?
        .read_to_end(&mut output_lines)
        .await?;
        let output_lines = String::from_utf8(output_lines)?;

        let diff = diff_lines(&result_lines, &output_lines)
            .set_diff_only(true)
            .names("Expected", "Actual");
        let is_different = diff.diff().iter().any(|d| !matches!(d, DiffOp::Equal(_)));
        if is_different {
            println!("Result unexpected, path:{:?}", path.as_ref());
            println!(
                "Hint: compare them with \"diff {} {}\"\n",
                path.as_ref()
                    .with_extension(&self.config.output_result_extension)
                    .display(),
                path.as_ref()
                    .with_extension(&self.config.expect_result_extension)
                    .display()
            )
        }

        Ok(is_different)
    }
}
