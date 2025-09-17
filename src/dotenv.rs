use smol_str::SmolStr;
use std::collections::HashMap;
use std::env::current_dir;
use std::fs;
use std::path::{Path, PathBuf};

// Define an enum for environment types
#[derive(Debug, PartialEq)]
pub enum EnvArgType {
    RspHeader,
    ReqHeader,
    Env,
    Secrets,
    KvStore
}

#[derive(Debug, PartialEq)]
enum EnvArgFileType {
    RspHeaders,
    ReqHeaders,
    Variables,
    Secrets,
    DotEnv,
    KvStore
}

impl From<EnvArgType> for EnvArgFileType {
    fn from(env_arg_type: EnvArgType) -> Self {
        match env_arg_type {
            EnvArgType::RspHeader => EnvArgFileType::RspHeaders,
            EnvArgType::ReqHeader => EnvArgFileType::ReqHeaders,
            EnvArgType::Env => EnvArgFileType::Variables,
            EnvArgType::Secrets => EnvArgFileType::Secrets,
            EnvArgType::KvStore => EnvArgFileType::KvStore
        }
    }
}

pub struct DotEnvInjector {
    file_path: PathBuf,
}

impl DotEnvInjector {
    pub fn new(file_path: Option<PathBuf>) -> Self {
        let file_path = file_path.unwrap_or_else(|| current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf()));
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

        // Read and merge specific .env.{variables|req_headers|rsp_headers|secrets} file variables
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
            EnvArgType::ReqHeader => "FASTEDGE_VAR_REQ_HEADER_",
            EnvArgType::Env => "FASTEDGE_VAR_ENV_",
            EnvArgType::Secrets => "FASTEDGE_VAR_SECRET_",
            EnvArgType::KvStore => "FASTEDGE_VAR_KV_STORE"
        };

        if key.starts_with("FASTEDGE_VAR_") {
            if key.starts_with(env_arg_prefix_str) {
                Some(SmolStr::new(key.trim_start_matches(env_arg_prefix_str)))
            } else {
                None
            }
        } else if *env_arg_type == EnvArgType::Env {
            Some(SmolStr::new(key))
        } else {
            None
        }
    }

    fn read_dotenv_file(
        &self,
        env_arg_file_type: EnvArgFileType,
    ) -> Result<HashMap<SmolStr, SmolStr>, std::io::Error> {
        let env_arg_file_type_str = match env_arg_file_type {
            EnvArgFileType::RspHeaders => ".env.rsp_headers",
            EnvArgFileType::ReqHeaders => ".env.req_headers",
            EnvArgFileType::Variables => ".env.variables",
            EnvArgFileType::Secrets => ".env.secrets",
            EnvArgFileType::DotEnv => ".env",
            EnvArgFileType::KvStore => ".env.kv_stores"
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
                    // Insert the key-value pair into the HashMap
                    variables.insert(
                        self.strip_quoted_strings(key),
                        self.strip_quoted_strings(value),
                    );
                }
            }
        }
        Ok(variables)
    }

    fn strip_quoted_strings(&self, value: &str) -> SmolStr {
        SmolStr::new(value.trim().trim_matches(|c| c == '"' || c == '\''))
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
            KEY3==value3
            KEY4=\"value4\"
            'KEY5'='value5'
            KEY6=\"some value with spaces\"
            KEY7=\"some \"edge\" case with = and `quotes`\"
            ",
        )
        .unwrap();

        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));
        let result = injector.read_dotenv_file(EnvArgFileType::DotEnv).unwrap();

        let mut expected = HashMap::new();
        expected.insert(SmolStr::new("KEY1"), SmolStr::new("value1"));
        expected.insert(SmolStr::new("KEY2"), SmolStr::new("value2"));
        expected.insert(SmolStr::new("KEY3"), SmolStr::new("=value3"));
        expected.insert(SmolStr::new("KEY4"), SmolStr::new("value4"));
        expected.insert(SmolStr::new("KEY5"), SmolStr::new("value5"));
        expected.insert(SmolStr::new("KEY6"), SmolStr::new("some value with spaces"));
        expected.insert(
            SmolStr::new("KEY7"),
            SmolStr::new("some \"edge\" case with = and `quotes`"),
        );

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

        // Invoking merge_with_dotenv_variables dsiabled via "should_read_dotenv=false"
        // Does nothing and only pass back the args_variables
        let result = injector.merge_with_dotenv_variables(false, EnvArgType::Env, args_variables);

        assert_eq!(result, expected);
    }

    #[test]
    fn test_merge_with_dotenv_variables_with_no_values_or_file() {
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
               KEY3=\"my own string value\"
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

        let variables_file_path = temp_dir.path().join(".env.variables");
        fs::write(
                  &variables_file_path,
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
        let temp_dir = tempdir().unwrap();
        let env_file_path = temp_dir.path().join(".env");
        fs::write(
            &env_file_path,
            "\
                KEY1=value1  # Ignore this line as it is not a rsp_header
                KEY2=value2  # Ignore  this line as it is not a rsp_header
                FASTEDGE_VAR_RSP_HEADER_KEY3=rsp_header3
                FASTEDGE_VAR_RSP_HEADER_KEY4=rsp_header4
                FASTEDGE_VAR_SECRET_KEY5=secret5  # Ignore this line as it is not a rsp_header
                ",
        )
        .unwrap();

        let rsp_headers_file_path = temp_dir.path().join(".env.rsp_headers");
        fs::write(
                  &rsp_headers_file_path,
                  "\
                KEY6=rsp_header6
                KEY7=rsp_header7
                FASTEDGE_VAR_RSP_HEADER_KEY8=value8  # Ignore this line as it starts with FASTEDGE_VAR_ prefix ( only allowed in .env )
                ",
              )
              .unwrap();

        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));

        let mut headers_variables = HashMap::new(); //rename these across all tests
        headers_variables.insert(SmolStr::new("KEY4"), SmolStr::new("override_rsp_header4"));
        headers_variables.insert(SmolStr::new("KEY6"), SmolStr::new("override_rsp_header6"));

        let result =
            injector.merge_with_dotenv_variables(true, EnvArgType::RspHeader, headers_variables);

        let mut expected = HashMap::new();
        expected.insert(SmolStr::new("KEY3"), SmolStr::new("rsp_header3"));
        expected.insert(SmolStr::new("KEY4"), SmolStr::new("override_rsp_header4"));
        expected.insert(SmolStr::new("KEY6"), SmolStr::new("override_rsp_header6"));
        expected.insert(SmolStr::new("KEY7"), SmolStr::new("rsp_header7"));

        assert_eq!(result, expected);
    }

    #[test]
    fn test_merge_with_dotenv_variables_and_dotenv_secrets() {
        let temp_dir = tempdir().unwrap();
        let env_file_path = temp_dir.path().join(".env");
        fs::write(
            &env_file_path,
            "\
                KEY1=value1  # Ignore this line as it is not a secret
                KEY2=value2  # Ignore  this line as it is not a secret
                FASTEDGE_VAR_SECRET_KEY3=secret3
                FASTEDGE_VAR_SECRET_KEY4=secret4
                FASTEDGE_VAR_ENV_KEY5=value5 # Ignore this line as it is not a secret
                ",
        )
        .unwrap();

        let secrets_file_path = temp_dir.path().join(".env.secrets");
        fs::write(
               &secrets_file_path,
               "\
                KEY6=secret6
                KEY7=secret7
                FASTEDGE_VAR_SECRET_KEY8=value8  # Ignore this line as it starts with FASTEDGE_VAR_ prefix ( only allowed in .env )
                FASTEDGE_VAR_RSP_HEADER_KEY9=value9  # Ignore this line as it starts with FASTEDGE_VAR_ prefix ( only allowed in .env )
                ",
           )
           .unwrap();

        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));

        let mut secret_variables = HashMap::new();
        secret_variables.insert(SmolStr::new("KEY4"), SmolStr::new("override_secret4"));
        secret_variables.insert(SmolStr::new("KEY6"), SmolStr::new("override_secret6"));
        secret_variables.insert(SmolStr::new("ARGSECRET_ONLY"), SmolStr::new("new_secret8"));

        let result =
            injector.merge_with_dotenv_variables(true, EnvArgType::Secrets, secret_variables);

        let mut expected = HashMap::new();
        expected.insert(SmolStr::new("KEY3"), SmolStr::new("secret3"));
        expected.insert(SmolStr::new("KEY4"), SmolStr::new("override_secret4"));
        expected.insert(SmolStr::new("KEY6"), SmolStr::new("override_secret6"));
        expected.insert(SmolStr::new("KEY7"), SmolStr::new("secret7"));
        expected.insert(SmolStr::new("ARGSECRET_ONLY"), SmolStr::new("new_secret8"));

        assert_eq!(result, expected);
    }

    #[test]
    fn test_merge_with_dotenv_variables_and_dotenv_req_headers() {
        let temp_dir = tempdir().unwrap();
        let env_file_path = temp_dir.path().join(".env");
        fs::write(
               &env_file_path,
               "\
                KEY1=value1  # Ignore this line as it is not a req_header
                KEY2=value2  # Ignore  this line as it is not a req_header
                FASTEDGE_VAR_REQ_HEADER_KEY3=header3
                FASTEDGE_VAR_SECRET_KEY4=secret4 # Ignore  this line as it is not a req_header
                FASTEDGE_VAR_RSP_HEADER_KEY5=rsp_header5 # Ignore  this line as it is not a req_header
                FASTEDGE_VAR_ENV_KEY6=value6 # Ignore  this line as it is not a req_header
                FASTEDGE_VAR_REQ_HEADER_KEY7=header7
                ",
           )
           .unwrap();

        let secrets_file_path = temp_dir.path().join(".env.req_headers");
        fs::write(
               &secrets_file_path,
               "\
                KEY8=header8
                KEY9=header9
                FASTEDGE_VAR_REQ_HEADER_KEY10=req_header10  # Ignore this line as it starts with FASTEDGE_VAR_ prefix ( only allowed in .env )
                FASTEDGE_VAR_RSP_HEADER_KEY11=rsp_header11  # Ignore this line as it starts with FASTEDGE_VAR_ prefix ( only allowed in .env )
                ",
           )
           .unwrap();

        let injector = DotEnvInjector::new(Some(temp_dir.path().to_path_buf()));

        let mut req_header_variables = HashMap::new();
        req_header_variables.insert(SmolStr::new("KEY7"), SmolStr::new("override_header7"));
        req_header_variables.insert(SmolStr::new("KEY8"), SmolStr::new("override_header8"));
        req_header_variables.insert(SmolStr::new("ARGSECRET_ONLY"), SmolStr::new("header12"));

        let result =
            injector.merge_with_dotenv_variables(true, EnvArgType::ReqHeader, req_header_variables);

        let mut expected = HashMap::new();
        expected.insert(SmolStr::new("KEY3"), SmolStr::new("header3"));
        expected.insert(SmolStr::new("KEY7"), SmolStr::new("override_header7"));
        expected.insert(SmolStr::new("KEY8"), SmolStr::new("override_header8"));
        expected.insert(SmolStr::new("KEY9"), SmolStr::new("header9"));
        expected.insert(SmolStr::new("ARGSECRET_ONLY"), SmolStr::new("header12"));

        assert_eq!(result, expected);
    }
}
