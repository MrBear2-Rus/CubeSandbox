use crate::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};

/// `POST /init` request. Upstream envd also sends `volumeMounts`,
/// `accessToken`, `timestamp`, `hyperloopIP`, and `lifecycleID`; those are
/// accepted and ignored here because CubeSandbox delegates them elsewhere.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitRequest {
    #[serde(default)]
    env_vars: HashMap<String, String>,
    #[serde(default)]
    default_user: Option<String>,
    #[serde(default)]
    default_workdir: Option<String>,
}

pub async fn init(
    State(state): State<Arc<AppState>>,
    Json(request): Json<InitRequest>,
) -> Response {
    let mut defaults = state
        .defaults
        .read()
        .expect("defaults lock poisoned")
        .clone();
    defaults.env_vars.extend(request.env_vars);
    if let Some(user) = request.default_user.filter(|user| !user.is_empty()) {
        defaults.user = Some(user);
    }
    if let Some(workdir) = request
        .default_workdir
        .filter(|workdir| !workdir.is_empty())
    {
        defaults.workdir = Some(workdir);
    }
    *state.defaults.write().expect("defaults lock poisoned") = defaults;
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /envs` returns the environment variables established by `/init`,
/// matching upstream envd's `EnvVars` response.
pub async fn envs(State(state): State<Arc<AppState>>) -> Json<HashMap<String, String>> {
    let env_vars = state
        .defaults
        .read()
        .expect("defaults lock poisoned")
        .env_vars
        .clone();
    Json(env_vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_request_decodes_camel_case() {
        let request: InitRequest =
            serde_json::from_slice(br#"{"envVars":{"FOO":"bar"},"defaultWorkdir":"/tmp"}"#)
                .expect("init request decodes");
        assert_eq!(request.env_vars.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(request.default_workdir.as_deref(), Some("/tmp"));
    }

    #[tokio::test]
    async fn init_stores_env_vars_and_default_workdir() {
        let state = Arc::new(AppState::new(49983));
        let request = InitRequest {
            env_vars: HashMap::from([("SESSION_ID".to_owned(), "abc".to_owned())]),
            default_user: None,
            default_workdir: Some("/workspace".to_owned()),
        };

        let response = init(State(state.clone()), Json(request)).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let defaults = state.defaults.read().unwrap().clone();
        assert_eq!(
            defaults.env_vars.get("SESSION_ID").map(String::as_str),
            Some("abc")
        );
        assert_eq!(defaults.workdir(), Some("/workspace".to_owned()));
    }

    #[tokio::test]
    async fn envs_returns_stored_env_vars() {
        let state = Arc::new(AppState::new(49983));
        state
            .defaults
            .write()
            .unwrap()
            .env_vars
            .insert("FOO".to_owned(), "bar".to_owned());

        let response = envs(State(state.clone())).await;
        assert_eq!(response.0.get("FOO").map(String::as_str), Some("bar"));
    }
}
