mod backend_client;
mod cli;
mod commands;
mod context;
mod diagnostics_bundle;
mod dto;
mod file_write;
mod first_party;
mod host_info;
mod operator_env;
mod render;
mod runtime;
mod serve_invocation;
mod setup_bundle;
mod tailscale;
mod webui_token;

fn main() -> anyhow::Result<()> {
    // Mirror the v1 binary's behavior so dev workflows can keep LLM
    // keys / base URLs in `.env`. Silent on missing file — production
    // hosts use shell-exported env or systemd unit env, not `.env` —
    // but any other error (parse failure, permission denied) is
    // surfaced to stderr so a malformed file does not boot the host
    // with stale env. The boot itself still proceeds because
    // operators may have already exported the same keys in their
    // shell.
    if let Err(error) = dotenvy::dotenv()
        && !error.not_found()
    {
        eprintln!("warning: failed to load .env: {error}");
    }
    cli::run()
}
