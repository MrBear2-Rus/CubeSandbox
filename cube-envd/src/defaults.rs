use std::collections::HashMap;

/// Process-wide defaults delivered by `POST /init` (upstream envd's
/// `envVars` / `defaultUser` / `defaultWorkdir`). New processes and PTYs merge
/// these in, with per-request values taking precedence.
#[derive(Clone, Debug, Default)]
pub(crate) struct Defaults {
    pub(crate) env_vars: HashMap<String, String>,
    pub(crate) user: Option<String>,
    pub(crate) workdir: Option<String>,
}

impl Defaults {
    pub(crate) fn user_or_root(&self) -> String {
        self.user
            .as_deref()
            .filter(|user| !user.is_empty())
            .unwrap_or("root")
            .to_owned()
    }

    pub(crate) fn workdir(&self) -> Option<String> {
        self.workdir
            .as_deref()
            .filter(|workdir| !workdir.is_empty())
            .map(str::to_owned)
    }

    /// Defaults first, then request env vars so the caller can override.
    pub(crate) fn merged_env_vars(
        &self,
        request: &HashMap<String, String>,
    ) -> HashMap<String, String> {
        let mut env_vars = self.env_vars.clone();
        env_vars.extend(
            request
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        env_vars
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_env_vars_override_defaults() {
        let defaults = Defaults {
            env_vars: HashMap::from([
                ("FOO".to_owned(), "default".to_owned()),
                ("KEEP".to_owned(), "default".to_owned()),
            ]),
            ..Defaults::default()
        };
        let request = HashMap::from([("FOO".to_owned(), "request".to_owned())]);
        let merged = defaults.merged_env_vars(&request);

        assert_eq!(merged.get("FOO").map(String::as_str), Some("request"));
        assert_eq!(merged.get("KEEP").map(String::as_str), Some("default"));
    }

    #[test]
    fn empty_user_and_workdir_fall_back() {
        let defaults = Defaults {
            user: Some(String::new()),
            workdir: Some(String::new()),
            ..Defaults::default()
        };
        assert_eq!(defaults.user_or_root(), "root");
        assert_eq!(defaults.workdir(), None);
    }
}
