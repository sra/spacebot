//! Exec tool for running subprocesses (task workers only).

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::process::Command;

/// Tool for executing subprocesses.
#[derive(Debug, Clone)]
pub struct ExecTool;

impl ExecTool {
    /// Create a new exec tool.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ExecTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Error type for exec tool.
#[derive(Debug, thiserror::Error)]
#[error("Execution failed: {message}")]
pub struct ExecError {
    message: String,
    exit_code: i32,
}

/// Arguments for exec tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecArgs {
    /// The program to execute.
    pub program: String,
    /// Arguments to pass to the program.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional working directory.
    pub working_dir: Option<String>,
    /// Environment variables to set (key-value pairs).
    #[serde(default)]
    pub env: Vec<EnvVar>,
    /// Timeout in seconds (default: 60).
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

/// Environment variable.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EnvVar {
    /// The variable name.
    pub key: String,
    /// The variable value.
    pub value: String,
}

fn default_timeout() -> u64 {
    60
}

/// Output from exec tool.
#[derive(Debug, Serialize)]
pub struct ExecOutput {
    /// Whether the execution succeeded.
    pub success: bool,
    /// The exit code.
    pub exit_code: i32,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Formatted summary.
    pub summary: String,
}

impl Tool for ExecTool {
    const NAME: &'static str = "exec";

    type Error = ExecError;
    type Args = ExecArgs;
    type Output = ExecOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Execute a subprocess with specific arguments and environment. This is more precise than shell for running specific programs. Use this for running compilers, formatters, test runners, or any external binary with specific arguments.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "program": {
                        "type": "string",
                        "description": "The program or binary to execute (e.g., 'cargo', 'python', 'node')"
                    },
                    "args": {
                        "type": "array",
                        "items": {
                            "type": "string"
                        },
                        "default": [],
                        "description": "Arguments to pass to the program"
                    },
                    "working_dir": {
                        "type": "string",
                        "description": "Optional working directory for the execution"
                    },
                    "env": {
                        "type": "array",
                        "description": "Environment variables to set",
                        "items": {
                            "type": "object",
                            "properties": {
                                "key": {
                                    "type": "string",
                                    "description": "Environment variable name"
                                },
                                "value": {
                                    "type": "string",
                                    "description": "Environment variable value"
                                }
                            },
                            "required": ["key", "value"]
                        }
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 300,
                        "default": 60,
                        "description": "Maximum time to wait (1-300 seconds)"
                    }
                },
                "required": ["program"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut cmd = Command::new(&args.program);
        cmd.args(&args.args);

        if let Some(dir) = args.working_dir {
            cmd.current_dir(dir);
        }

        for env_var in args.env {
            cmd.env(env_var.key, env_var.value);
        }

        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let timeout = tokio::time::Duration::from_secs(args.timeout_seconds);

        let output = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| ExecError {
                message: "Execution timed out".to_string(),
                exit_code: -1,
            })?
            .map_err(|e| ExecError {
                message: format!("Failed to execute: {e}"),
                exit_code: -1,
            })?;

        let stdout = crate::tools::truncate_output(
            &String::from_utf8_lossy(&output.stdout),
            crate::tools::MAX_TOOL_OUTPUT_BYTES,
        );
        let stderr = crate::tools::truncate_output(
            &String::from_utf8_lossy(&output.stderr),
            crate::tools::MAX_TOOL_OUTPUT_BYTES,
        );
        let exit_code = output.status.code().unwrap_or(-1);
        let success = output.status.success();

        let summary = format_exec_output(exit_code, &stdout, &stderr);

        Ok(ExecOutput {
            success,
            exit_code,
            stdout,
            stderr,
            summary,
        })
    }
}

/// Format exec output for display.
fn format_exec_output(exit_code: i32, stdout: &str, stderr: &str) -> String {
    let mut output = String::new();

    output.push_str(&format!("Exit code: {}\n", exit_code));

    if !stdout.is_empty() {
        output.push_str("\n--- STDOUT ---\n");
        output.push_str(stdout);
    }

    if !stderr.is_empty() {
        output.push_str("\n--- STDERR ---\n");
        output.push_str(stderr);
    }

    if stdout.is_empty() && stderr.is_empty() {
        output.push_str("\n[No output]\n");
    }

    output
}

/// Legacy function for backward compatibility.
pub async fn exec(
    program: &str,
    args: &[&str],
    working_dir: Option<&std::path::Path>,
    env: Option<&[(&str, &str)]>,
) -> crate::error::Result<ExecResult> {
    let tool = ExecTool::new();

    let env_vars: Vec<EnvVar> = env
        .map(|e| {
            e.iter()
                .map(|(k, v)| EnvVar {
                    key: k.to_string(),
                    value: v.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    let exec_args = ExecArgs {
        program: program.to_string(),
        args: args.iter().map(|&s| s.to_string()).collect(),
        working_dir: working_dir.map(|p| p.to_string_lossy().to_string()),
        env: env_vars,
        timeout_seconds: 60,
    };

    let output = tool
        .call(exec_args)
        .await
        .map_err(|e| crate::error::AgentError::Other(e.into()))?;

    Ok(ExecResult {
        success: output.success,
        exit_code: output.exit_code,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// Result of a subprocess execution.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

use anyhow::Context as _;
