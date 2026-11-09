// Inference settings use dedicated LLM endpoints. Keep this schema empty until
// any operator-config-backed keys are accepted by the Reborn config API.
export const INFERENCE_FIELDS = [];

export const RESTART_REQUIRED_KEYS = new Set([
  "tunnel.provider", "tunnel.public_url",
  "gateway.rate_limit", "gateway.max_connections",
]);
