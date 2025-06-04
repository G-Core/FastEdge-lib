use smol_str::SmolStr;
use std::collections::HashMap;
use std::env::current_dir;
use std::fs;
use std::path::{Path, PathBuf};

// Define an enum for environment types
#[derive(Debug, PartialEq)]
pub enum EnvArgType {
    RspHeader,
    Env,
    Secrets,
}

#[derive(Debug, PartialEq)]
enum EnvArgFileType {
    RspHeaders,
    Variables,
    Secrets,
    DotEnv,
}

impl From<EnvArgType> for EnvArgFileType {
    fn from(env_arg_type: EnvArgType) -> Self {
        match env_arg_type {
            EnvArgType::RspHeader => EnvArgFileType::RspHeaders,
            EnvArgType::Env => EnvArgFileType::Variables,
            EnvArgType::Secrets => EnvArgFileType::Secrets,
        }
    }
}

pub struct DotEnvInjector {
    file_path: PathBuf,
}

impl DotEnvInjector {
    pub fn new(file_path: Option<PathBuf>) -> Self {
        let file_path = match file_path {
            Some(path) => path,
            None => current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf()),
        };
        Self { file_path }
    }

    pub fn merge_with_dotenv_variables(
        &self,
        should_read_dotenv: bool,
        env_arg_type: EnvArgType,
        args_variables: HashMap<SmolStr, SmolStr>,
    ) -> HashMap<SmolStr, SmolStr> {
        if !should_read_dotenv {
            return args_variables;
        }

        let mut merged_variables = HashMap::new();

        // Start with the provided arguments
        for (key, value) in args_variables {
            merged_variables.insert(key.clone(), value.clone());
        }

        // Read and merge .env file variables
        if let Ok(dotenv_vars) = self.read_dotenv_file(EnvArgFileType::DotEnv) {
            for (key, value) in dotenv_vars {
                if let Some(filtered_key) = self.filter_named_keys(&key, &env_arg_type) {
                    merged_variables.entry(filtered_key).or_insert(value);
                }
            }
        }

        // Read and merge specific environment type variables
        let env_arg_file_type: EnvArgFileType = env_arg_type.into();
        if let Ok(dotenv_vars) = self.read_dotenv_file(env_arg_file_type) {
            for (key, value) in dotenv_vars {
                if !key.starts_with("FASTEDGE_VAR_") {
                    merged_variables.entry(key).or_insert(value);
                }
            }
        }

        merged_variables
    }

    fn filter_named_keys(&self, key: &SmolStr, env_arg_type: &EnvArgType) -> Option<SmolStr> {
        let env_arg_prefix_str = match env_arg_type {
            EnvArgType::RspHeader => "FASTEDGE_VAR_RSP_HEADER_",
            EnvArgType::Env => "FASTEDGE_VAR_ENV_",
            EnvArgType::Secrets => "FASTEDGE_VAR_SECRET_",
        };

        let stripped_key: Option<SmolStr> = if key.starts_with("FASTEDGE_VAR_") {
            if key.starts_with(env_arg_prefix_str) {
                Some(SmolStr::new(key.trim_start_matches(env_arg_prefix_str)))
            } else {
                None
            }
        } else {
            Some(SmolStr::new(key))
        };
        stripped_key
    }

    fn read_dotenv_file(
        &self,
        env_arg_file_type: EnvArgFileType,
    ) -> Result<HashMap<SmolStr, SmolStr>, std::io::Error> {
        let env_arg_file_type_str = match env_arg_file_type {
            EnvArgFileType::RspHeaders => ".env.rsp_headers",
            EnvArgFileType::Variables => ".env.variables",
            EnvArgFileType::Secrets => ".env.secrets",
            EnvArgFileType::DotEnv => ".env",
        };

        let filename = self.file_path.join(env_arg_file_type_str);
        let mut variables = HashMap::new();
        if let Ok(lines) = fs::read_to_string(filename) {
            for _line in lines.lines() {
                let line = match _line.split_once('#') {
                    Some((line, _)) => line,
                    None => _line,
                };
                if line.trim().is_empty() {
                    continue;
                }
                if let Some((key, value)) = line.split_once('=') {
                    variables.insert(SmolStr::new(key.trim()), SmolStr::new(value.trim()));
                }
            }
        }
        Ok(variables)
    }
}

#[cfg(test)]
mod dotenv_injector_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_read_dotenv_file() {
        // Create a temporary directory
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join(".env");
        fs::write(
            &file_path,
            "\
            KEY1=value1
            KEY2=value2
            ",
        )
        .unwrap();

        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));
        let result = injector.read_dotenv_file(EnvArgFileType::DotEnv).unwrap();

        let mut expected = HashMap::new();
        expected.insert(SmolStr::new("KEY1"), SmolStr::new("value1"));
        expected.insert(SmolStr::new("KEY2"), SmolStr::new("value2"));

        assert_eq!(result, expected);
    }

    #[test]
    fn test_read_dotenv_file_with_errored_lines() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join(".env");
        fs::write(
            &file_path,
            "\
          KEY1=value1
          This line is not valid
          KEY2=value2
          ",
        )
        .unwrap();

        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));
        let result = injector.read_dotenv_file(EnvArgFileType::DotEnv).unwrap();

        let mut expected = HashMap::new();
        expected.insert(SmolStr::new("KEY1"), SmolStr::new("value1"));
        expected.insert(SmolStr::new("KEY2"), SmolStr::new("value2"));

        assert_eq!(result, expected);
    }

    #[test]
    fn test_read_dotenv_secrets_file_with_comments() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join(".env.secrets");
        fs::write(
            &file_path,
            "\
                KEY3=value3
                # some_comment=value
                KEY4 = value4 # This is a comment - ignore it
                ",
        )
        .unwrap();

        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));
        let result = injector.read_dotenv_file(EnvArgFileType::Secrets).unwrap();

        let mut expected = HashMap::new();
        expected.insert(SmolStr::new("KEY3"), SmolStr::new("value3"));
        expected.insert(SmolStr::new("KEY4"), SmolStr::new("value4"));

        assert_eq!(result, expected);
    }

    #[test]
    fn test_read_dotenv_rsp_headers_file_with_comments_and_empty_lines() {
        let temp_dir = tempdir().unwrap();
        let file_path = temp_dir.path().join(".env.rsp_headers");
        fs::write(
            &file_path,
            "\
               #first line is rubbish

               KEY5=value5
               # another=comment_value

               KEY6=value6
               ",
        )
        .unwrap();

        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));
        let result = injector
            .read_dotenv_file(EnvArgFileType::RspHeaders)
            .unwrap();

        let mut expected = HashMap::new();
        expected.insert(SmolStr::new("KEY5"), SmolStr::new("value5"));
        expected.insert(SmolStr::new("KEY6"), SmolStr::new("value6"));

        assert_eq!(result, expected);
    }

    #[test]
    fn test_merge_with_dotenv_variables_disabled() {
        let temp_dir = tempdir().unwrap();
        let env_file_path = temp_dir.path().join(".env");
        fs::write(
            &env_file_path,
            "\
               KEY1=value1
               KEY2=value2 # this is a comment - ignore it
               ",
        )
        .unwrap();

        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));

        let mut args_variables = HashMap::new();
        args_variables.insert(SmolStr::new("KEY2"), SmolStr::new("override_value2"));
        args_variables.insert(SmolStr::new("KEY3"), SmolStr::new("value3"));

        let expected = args_variables.clone();

        // Call merge_with_dotenv_variables dsiabled via "should_read_dotenv=false"
        // Should do nothing and only pass back the args_variables
        let result = injector.merge_with_dotenv_variables(false, EnvArgType::Env, args_variables);

        assert_eq!(result, expected);
    }

    #[test]
    fn test_merge_with_dotenv_variables_with_no_values_or_file() {
        // Create a temporary directory
        let temp_dir = tempdir().unwrap();
        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));

        let args_variables = HashMap::new();
        let result = injector.merge_with_dotenv_variables(true, EnvArgType::Env, args_variables);
        let expected = HashMap::new();

        assert_eq!(result, expected);
    }

    #[test]
    fn test_merge_with_dotenv_variables() {
        let temp_dir = tempdir().unwrap();
        let env_file_path = temp_dir.path().join(".env");
        fs::write(
            &env_file_path,
            "\
               KEY1=value1 # This is a comment - ignore it
               KEY2=value2
               # KEY4=value4  This is also a comment
               ",
        )
        .unwrap();

        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));

        let mut args_variables = HashMap::new();
        args_variables.insert(SmolStr::new("KEY2"), SmolStr::new("override_value2"));
        args_variables.insert(SmolStr::new("KEY3"), SmolStr::new("value3"));

        let result = injector.merge_with_dotenv_variables(true, EnvArgType::Env, args_variables);

        let mut expected = HashMap::new();
        expected.insert(SmolStr::new("KEY1"), SmolStr::new("value1"));
        expected.insert(SmolStr::new("KEY2"), SmolStr::new("override_value2"));
        expected.insert(SmolStr::new("KEY3"), SmolStr::new("value3"));

        assert_eq!(result, expected);
    }

    #[test]
    fn test_merge_with_dotenv_variables_and_dotenv_variables() {
        // Create a temporary directory
        let temp_dir = tempdir().unwrap();
        let env_file_path = temp_dir.path().join(".env");
        fs::write(
               &env_file_path,
               "\
               KEY1=value1
               KEY2=value2
               FASTEDGE_VAR_ENV_KEY3=value3
               FASTEDGE_VAR_SECRET_KEY6=secret26  # Ignore this line as it starts with FASTEDGE_VAR_SECRET_ prefix
               ",
           )
           .unwrap();

        let secrets_file_path = temp_dir.path().join(".env.variables");
        fs::write(
               &secrets_file_path,
               "\
               KEY4=value4
               KEY5=value5
               FASTEDGE_VAR_ENV_KEY6=value6  # Ignore this line as it starts with FASTEDGE_VAR_ prefix ( only allowed in .env )
               ",
           )
           .unwrap();

        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));

        let mut args_variables = HashMap::new();
        args_variables.insert(SmolStr::new("KEY2"), SmolStr::new("override_value2"));
        args_variables.insert(SmolStr::new("KEY4"), SmolStr::new("override_value4"));

        let result = injector.merge_with_dotenv_variables(true, EnvArgType::Env, args_variables);

        let mut expected = HashMap::new();
        expected.insert(SmolStr::new("KEY1"), SmolStr::new("value1"));
        expected.insert(SmolStr::new("KEY2"), SmolStr::new("override_value2"));
        expected.insert(SmolStr::new("KEY3"), SmolStr::new("value3"));
        expected.insert(SmolStr::new("KEY4"), SmolStr::new("override_value4"));
        expected.insert(SmolStr::new("KEY5"), SmolStr::new("value5"));

        assert_eq!(result, expected);
    }

    #[test]
    fn test_merge_with_dotenv_variables_and_dotenv_rsp_headers() {
        // Create a temporary directory
        let temp_dir = tempdir().unwrap();
        let env_file_path = temp_dir.path().join(".env");
        fs::write(
               &env_file_path,
               "\
             KEY1=rsp_header1
             KEY2=rsp_header2
             FASTEDGE_VAR_SECRET_KEY6=secret26  # Ignore this line as it starts with FASTEDGE_VAR_SECRET_ prefix
             ",
           )
           .unwrap();

        let secrets_file_path = temp_dir.path().join(".env.rsp_headers");
        fs::write(
               &secrets_file_path,
               "\
             KEY3=rsp_header3
             KEY4=rsp_header4
             FASTEDGE_VAR_RSP_HEADER_KEY6=value6  # Ignore this line as it starts with FASTEDGE_VAR_ prefix ( only allowed in .env )
             ",
           )
           .unwrap();

        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));

        let mut args_variables = HashMap::new();
        args_variables.insert(SmolStr::new("KEY2"), SmolStr::new("override_rsp_header2"));
        args_variables.insert(SmolStr::new("KEY3"), SmolStr::new("override_rsp_header3"));

        let result =
            injector.merge_with_dotenv_variables(true, EnvArgType::RspHeader, args_variables);

        let mut expected = HashMap::new();
        expected.insert(SmolStr::new("KEY1"), SmolStr::new("rsp_header1"));
        expected.insert(SmolStr::new("KEY2"), SmolStr::new("override_rsp_header2"));
        expected.insert(SmolStr::new("KEY3"), SmolStr::new("override_rsp_header3"));
        expected.insert(SmolStr::new("KEY4"), SmolStr::new("rsp_header4"));

        assert_eq!(result, expected);
    }

    #[test]
    fn test_merge_with_dotenv_variables_and_dotenv_secrets() {
        // Create a temporary directory
        let temp_dir = tempdir().unwrap();
        let env_file_path = temp_dir.path().join(".env");
        fs::write(
               &env_file_path,
               "\
             KEY1=secret1
             KEY2=secret2
             FASTEDGE_VAR_ENV_KEY6=value6  # Ignore this line as it starts with FASTEDGE_VAR_RSP_HEADER_ prefix
             FASTEDGE_VAR_RSP_HEADER_KEY7=header7  # Ignore this line as it starts with FASTEDGE_VAR_RSP_HEADER_ prefix
             ",
           )
           .unwrap();

        let secrets_file_path = temp_dir.path().join(".env.secrets");
        fs::write(
            &secrets_file_path,
            "\
             KEY3=secret3
             KEY4=secret4
             ",
        )
        .unwrap();

        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));

        let mut args_variables = HashMap::new();
        args_variables.insert(SmolStr::new("KEY2"), SmolStr::new("override_secret2"));
        args_variables.insert(SmolStr::new("KEY3"), SmolStr::new("override_secret3"));

        let result =
            injector.merge_with_dotenv_variables(true, EnvArgType::Secrets, args_variables);

        let mut expected = HashMap::new();
        expected.insert(SmolStr::new("KEY1"), SmolStr::new("secret1"));
        expected.insert(SmolStr::new("KEY2"), SmolStr::new("override_secret2"));
        expected.insert(SmolStr::new("KEY3"), SmolStr::new("override_secret3"));
        expected.insert(SmolStr::new("KEY4"), SmolStr::new("secret4"));

        assert_eq!(result, expected);
    }
}
