//! Provider presets shared by the setup command and the daemon.
//!
//! Pure data: the CLI writes them into config.toml, the daemon reads them back.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI-compatible /chat/completions streaming (OpenAI, Google, Groq, DeepSeek, xAI, ...).
    OpenAiCompatible,
    /// Anthropic native /v1/messages streaming.
    Anthropic,
    /// Local Ollama-compatible server; no API key required.
    Ollama,
    /// GitHub Copilot: an OpenAI-compatible endpoint reached with a bearer the daemon mints
    /// from a GitHub token, plus the editor headers the integrator allowlist keys off.
    Copilot,
    /// AWS Bedrock: the Anthropic Messages body, SigV4 signing and binary event-stream frames.
    Bedrock,
    /// Google Vertex AI: an OAuth2 bearer, `:streamRawPredict` for Claude and the
    /// OpenAI-compatible surface for everything else.
    Vertex,
    /// ChatGPT / Codex subscription: the Responses API behind the Codex backend.
    Codex,
    /// An external agent spoken to over the Agent Client Protocol on stdio.
    Acp,
    /// OpenCode Zen / Go: one host that serves each model on the API its upstream speaks,
    /// so the transport is chosen per model.
    OpenCode,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::OpenAiCompatible => "openai_compatible",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Ollama => "ollama",
            ProviderKind::Copilot => "copilot",
            ProviderKind::Bedrock => "bedrock",
            ProviderKind::Vertex => "vertex",
            ProviderKind::Codex => "codex",
            ProviderKind::Acp => "acp",
            ProviderKind::OpenCode => "opencode",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "openai_compatible" | "openai-compatible" | "openai" => {
                Some(ProviderKind::OpenAiCompatible)
            }
            "anthropic" => Some(ProviderKind::Anthropic),
            "ollama" => Some(ProviderKind::Ollama),
            "copilot" => Some(ProviderKind::Copilot),
            "bedrock" => Some(ProviderKind::Bedrock),
            "vertex" => Some(ProviderKind::Vertex),
            "codex" => Some(ProviderKind::Codex),
            "acp" => Some(ProviderKind::Acp),
            "opencode" => Some(ProviderKind::OpenCode),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: ProviderKind,
    pub base_url: &'static str,
    pub default_model: &'static str,
    pub api_key_env: &'static str,
    pub requires_key: bool,
    pub signup_url: &'static str,
    /// Whether the daemon can sign in to this provider with an OAuth flow, so
    /// '/login' can offer a browser sign-in instead of an API key.
    pub oauth: bool,
}

/// An external agent mode: the preset spawns the mode's own CLI over the Agent Client Protocol,
/// resolved at run time from the CLI's install dir. No key of ours is needed (the CLI holds the
/// credentials) and `base_url` is only ever a command override.
const fn acp(id: &'static str, label: &'static str) -> ProviderPreset {
    ProviderPreset {
        id,
        label,
        kind: ProviderKind::Acp,
        base_url: "",
        default_model: id,
        api_key_env: "",
        requires_key: false,
        signup_url: "",
        oauth: false,
    }
}

/// Like [`acp`], but the preset answers as a differently-named model id: MiniMax Code's CLI mode
/// is `minimax`.
const fn acp_model(id: &'static str, label: &'static str, model: &'static str) -> ProviderPreset {
    ProviderPreset {
        default_model: model,
        ..acp(id, label)
    }
}

/// The preset catalog. 'custom' has empty base_url/model so the user supplies both.
pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "openai",
        label: "OpenAI",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.openai.com/v1",
        default_model: "gpt-5.5",
        api_key_env: "OPENAI_API_KEY",
        requires_key: true,
        signup_url: "https://platform.openai.com/api-keys",
        oauth: false,
    },
    ProviderPreset {
        id: "anthropic",
        label: "Anthropic (Claude)",
        kind: ProviderKind::Anthropic,
        base_url: "https://api.anthropic.com/v1",
        default_model: "claude-sonnet-4-5",
        api_key_env: "ANTHROPIC_API_KEY",
        requires_key: true,
        signup_url: "https://console.anthropic.com/settings/keys",
        oauth: false,
    },
    ProviderPreset {
        id: "google",
        label: "Google Gemini",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai/",
        default_model: "gemini-2.5-flash",
        api_key_env: "GEMINI_API_KEY",
        requires_key: true,
        signup_url: "https://aistudio.google.com/app/apikey",
        oauth: false,
    },
    ProviderPreset {
        id: "openrouter",
        label: "OpenRouter",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://openrouter.ai/api/v1",
        default_model: "openai/gpt-4o-mini",
        api_key_env: "OPENROUTER_API_KEY",
        requires_key: true,
        signup_url: "https://openrouter.ai/keys",
        oauth: true,
    },
    ProviderPreset {
        id: "groq",
        label: "Groq",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.groq.com/openai/v1",
        default_model: "llama-3.3-70b-versatile",
        api_key_env: "GROQ_API_KEY",
        requires_key: true,
        signup_url: "https://console.groq.com/keys",
        oauth: false,
    },
    ProviderPreset {
        id: "deepseek",
        label: "DeepSeek",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.deepseek.com/v1",
        default_model: "deepseek-flash",
        api_key_env: "DEEPSEEK_API_KEY",
        requires_key: true,
        signup_url: "https://platform.deepseek.com/api_keys",
        oauth: false,
    },
    ProviderPreset {
        id: "xai",
        label: "xAI (Grok)",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.x.ai/v1",
        default_model: "grok-4.5",
        api_key_env: "XAI_API_KEY",
        requires_key: true,
        signup_url: "https://console.x.ai",
        oauth: false,
    },
    ProviderPreset {
        id: "mistral",
        label: "Mistral",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.mistral.ai/v1",
        default_model: "mistral-large-latest",
        api_key_env: "MISTRAL_API_KEY",
        requires_key: true,
        signup_url: "https://console.mistral.ai/api-keys",
        oauth: false,
    },
    ProviderPreset {
        id: "together",
        label: "Together AI",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.together.xyz/v1",
        default_model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        api_key_env: "TOGETHER_API_KEY",
        requires_key: true,
        signup_url: "https://api.together.xyz/settings/api-keys",
        oauth: false,
    },
    ProviderPreset {
        id: "nous",
        label: "Nous Portal",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://inference-api.nousresearch.com/v1",
        // The portal serves its catalog live, so the model is chosen after sign-in
        // (the activation probe picks one, or '/model' names it).
        default_model: "",
        api_key_env: "NOUS_API_KEY",
        requires_key: true,
        signup_url: "https://portal.nousresearch.com",
        oauth: true,
    },
    ProviderPreset {
        id: "moonshot",
        label: "Moonshot (Kimi)",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.moonshot.ai/v1",
        default_model: "kimi-k2-turbo-preview",
        api_key_env: "MOONSHOT_API_KEY",
        requires_key: true,
        signup_url: "https://platform.moonshot.ai/console/api-keys",
        oauth: false,
    },
    ProviderPreset {
        id: "moonshot-cn",
        label: "Moonshot (China)",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.moonshot.cn/v1",
        default_model: "",
        api_key_env: "MOONSHOT_CN_API_KEY",
        requires_key: true,
        signup_url: "",
        oauth: false,
    },
    ProviderPreset {
        id: "zai",
        label: "Z.AI (GLM)",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.z.ai/api/paas/v4",
        default_model: "glm-4.6",
        api_key_env: "ZAI_API_KEY",
        requires_key: true,
        signup_url: "https://z.ai/manage-apikey/apikey-list",
        oauth: false,
    },
    ProviderPreset {
        id: "cerebras",
        label: "Cerebras",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.cerebras.ai/v1",
        default_model: "",
        api_key_env: "CEREBRAS_API_KEY",
        requires_key: true,
        signup_url: "https://cloud.cerebras.ai",
        oauth: false,
    },
    ProviderPreset {
        id: "fireworks",
        label: "Fireworks AI",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.fireworks.ai/inference/v1",
        default_model: "",
        api_key_env: "FIREWORKS_API_KEY",
        requires_key: true,
        signup_url: "https://app.fireworks.ai/settings/users/api-keys",
        oauth: false,
    },
    ProviderPreset {
        id: "deepinfra",
        label: "DeepInfra",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.deepinfra.com/v1/openai",
        default_model: "",
        api_key_env: "DEEPINFRA_API_KEY",
        requires_key: true,
        signup_url: "https://deepinfra.com/dash/api_keys",
        oauth: false,
    },
    ProviderPreset {
        id: "nvidia",
        label: "NVIDIA NIM",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://integrate.api.nvidia.com/v1",
        default_model: "",
        api_key_env: "NVIDIA_API_KEY",
        requires_key: true,
        signup_url: "https://build.nvidia.com",
        oauth: false,
    },
    ProviderPreset {
        id: "novita",
        label: "Novita AI",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.novita.ai/openai/v1",
        default_model: "",
        api_key_env: "NOVITA_API_KEY",
        requires_key: true,
        signup_url: "https://novita.ai/settings/key-management",
        oauth: false,
    },
    ProviderPreset {
        id: "nebius",
        label: "Nebius Token Factory",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.tokenfactory.nebius.com/v1",
        default_model: "",
        api_key_env: "NEBIUS_API_KEY",
        requires_key: true,
        signup_url: "https://tokenfactory.nebius.com",
        oauth: false,
    },
    ProviderPreset {
        id: "huggingface",
        label: "Hugging Face Router",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://router.huggingface.co/v1",
        default_model: "",
        api_key_env: "HF_TOKEN",
        requires_key: true,
        signup_url: "https://huggingface.co/settings/tokens",
        oauth: false,
    },
    ProviderPreset {
        id: "ai-gateway",
        label: "Vercel AI Gateway",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://ai-gateway.vercel.sh/v1",
        default_model: "",
        api_key_env: "AI_GATEWAY_API_KEY",
        requires_key: true,
        signup_url: "https://vercel.com/dashboard/ai-gateway",
        oauth: false,
    },
    ProviderPreset {
        id: "alibaba",
        label: "Alibaba DashScope (Qwen)",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        default_model: "qwen-plus",
        api_key_env: "DASHSCOPE_API_KEY",
        requires_key: true,
        signup_url: "https://bailian.console.alibabacloud.com",
        oauth: false,
    },
    ProviderPreset {
        id: "alibaba-coding-plan",
        label: "Alibaba Cloud (Coding Plan)",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://coding-intl.dashscope.aliyuncs.com/v1",
        default_model: "",
        api_key_env: "ALIBABA_CODING_PLAN_API_KEY",
        requires_key: true,
        signup_url: "https://help.aliyun.com/zh/model-studio/",
        oauth: false,
    },
    ProviderPreset {
        id: "minimax",
        label: "MiniMax",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.minimax.io/v1",
        default_model: "",
        api_key_env: "MINIMAX_API_KEY",
        requires_key: true,
        signup_url: "https://platform.minimax.io",
        oauth: false,
    },
    ProviderPreset {
        id: "minimax-cn",
        label: "MiniMax (China)",
        kind: ProviderKind::Anthropic,
        base_url: "https://api.minimaxi.com/anthropic",
        default_model: "",
        api_key_env: "MINIMAX_CN_API_KEY",
        requires_key: true,
        signup_url: "",
        oauth: false,
    },
    ProviderPreset {
        id: "upstage",
        label: "Upstage Solar",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.upstage.ai/v1",
        default_model: "",
        api_key_env: "UPSTAGE_API_KEY",
        requires_key: true,
        signup_url: "https://console.upstage.ai/api-keys",
        oauth: false,
    },
    ProviderPreset {
        id: "arcee",
        label: "Arcee AI",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.arcee.ai/api/v1",
        default_model: "",
        api_key_env: "ARCEEAI_API_KEY",
        requires_key: true,
        signup_url: "https://models.arcee.ai",
        oauth: false,
    },
    ProviderPreset {
        id: "gmi",
        label: "GMI Cloud",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.gmi-serving.com/v1",
        default_model: "",
        api_key_env: "GMI_API_KEY",
        requires_key: true,
        signup_url: "https://console.gmicloud.ai",
        oauth: false,
    },
    ProviderPreset {
        id: "kilocode",
        label: "Kilo Code Gateway",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.kilo.ai/api/gateway",
        default_model: "",
        api_key_env: "KILOCODE_API_KEY",
        requires_key: true,
        signup_url: "https://app.kilo.ai",
        oauth: false,
    },
    ProviderPreset {
        id: "opencode-zen",
        label: "OpenCode Zen",
        kind: ProviderKind::OpenCode,
        base_url: "https://opencode.ai/zen/v1",
        default_model: "",
        api_key_env: "OPENCODE_ZEN_API_KEY",
        // OpenCode discontinued anonymous free-tier access to the relay; Zen now needs its own key.
        requires_key: true,
        signup_url: "https://opencode.ai/auth",
        oauth: false,
    },
    ProviderPreset {
        id: "opencode-go",
        label: "OpenCode Go",
        // The gateway serves each model on its upstream's API; the OpenCode transport picks
        // the surface per model, so Qwen's Messages surface and Kimi's chat/completions
        // both work behind one preset.
        kind: ProviderKind::OpenCode,
        base_url: "https://opencode.ai/zen/go/v1",
        default_model: "kimi-k3",
        api_key_env: "OPENCODE_GO_API_KEY",
        requires_key: true,
        signup_url: "https://opencode.ai/auth",
        oauth: false,
    },
    ProviderPreset {
        id: "copilot",
        label: "GitHub Copilot (GitHub token)",
        kind: ProviderKind::Copilot,
        base_url: "https://api.githubcopilot.com",
        // Copilot serves its catalog live, and an Enterprise account's list differs from an
        // individual's, so the model is resolved after sign-in.
        default_model: "",
        // A GitHub token, not a Copilot key: `gh auth token` prints one, and the daemon
        // exchanges it for the short-lived Copilot bearer on every turn that needs one.
        api_key_env: "COPILOT_GITHUB_TOKEN",
        requires_key: true,
        signup_url: "https://github.com/settings/copilot",
        oauth: false,
    },
    ProviderPreset {
        id: "ollama-cloud",
        label: "Ollama Cloud",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://ollama.com/v1",
        default_model: "",
        api_key_env: "OLLAMA_API_KEY",
        requires_key: true,
        signup_url: "https://ollama.com/settings/keys",
        oauth: false,
    },
    ProviderPreset {
        id: "xiaomi",
        label: "Xiaomi MiMo",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.xiaomimimo.com/v1",
        default_model: "",
        api_key_env: "XIAOMI_API_KEY",
        requires_key: true,
        signup_url: "https://xiaomimimo.com",
        oauth: false,
    },
    ProviderPreset {
        id: "stepfun",
        label: "StepFun Step Plan",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.stepfun.ai/step_plan/v1",
        default_model: "",
        api_key_env: "STEPFUN_API_KEY",
        requires_key: true,
        signup_url: "",
        oauth: false,
    },
    ProviderPreset {
        id: "tencent-tokenhub",
        label: "Tencent TokenHub",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://tokenhub.tencentmaas.com/v1",
        default_model: "",
        api_key_env: "TOKENHUB_API_KEY",
        requires_key: true,
        signup_url: "",
        oauth: false,
    },
    ProviderPreset {
        id: "tencent-tokenplan",
        label: "Tencent TokenPlan",
        kind: ProviderKind::Anthropic,
        base_url: "https://api.lkeap.cloud.tencent.com/plan/anthropic",
        default_model: "",
        api_key_env: "TOKENPLAN_API_KEY",
        requires_key: true,
        signup_url: "",
        oauth: false,
    },
    ProviderPreset {
        id: "qwen-portal",
        label: "Qwen Portal",
        kind: ProviderKind::OpenAiCompatible,
        // Upstream reaches this endpoint through the Qwen CLI's OAuth login; here the
        // portal key is what authenticates it.
        base_url: "https://portal.qwen.ai/v1",
        default_model: "",
        api_key_env: "QWEN_API_KEY",
        requires_key: true,
        signup_url: "https://portal.qwen.ai",
        oauth: false,
    },
    ProviderPreset {
        id: "meta-ai",
        label: "Meta Model API",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.meta.ai/v1",
        default_model: "",
        api_key_env: "META_API_KEY",
        requires_key: true,
        signup_url: "https://developer.meta.com/ai/",
        oauth: false,
    },
    ProviderPreset {
        id: "router",
        label: "Ramp Router",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.router.com/v1",
        default_model: "",
        api_key_env: "ROUTER_API_KEY",
        requires_key: true,
        signup_url: "https://app.router.com/keys",
        oauth: false,
    },
    ProviderPreset {
        id: "commandcode",
        label: "CommandCode",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.commandcode.ai/provider/v1",
        default_model: "",
        api_key_env: "COMMANDCODE_API_KEY",
        requires_key: true,
        signup_url: "https://commandcode.ai/",
        oauth: false,
    },
    ProviderPreset {
        id: "actual",
        label: "Actual Computer",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "https://api.actual.inc/v1",
        default_model: "",
        api_key_env: "ACTUAL_API_KEY",
        requires_key: true,
        signup_url: "https://actual.inc",
        oauth: false,
    },
    ProviderPreset {
        id: "azure-foundry",
        label: "Azure Foundry",
        kind: ProviderKind::OpenAiCompatible,
        // The endpoint is per-deployment, so it carries no default base URL: '/login'
        // stores one, or config.toml sets model.base_url.
        base_url: "",
        default_model: "",
        api_key_env: "AZURE_FOUNDRY_API_KEY",
        requires_key: true,
        signup_url: "https://ai.azure.com/",
        oauth: false,
    },
    ProviderPreset {
        id: "bedrock",
        label: "AWS Bedrock",
        kind: ProviderKind::Bedrock,
        // The region decides the host, so the endpoint is resolved per request; an explicit
        // base URL (a VPC endpoint) still wins when one is stored.
        base_url: "",
        default_model: "",
        // The credential is "access-key:secret[:session-token]" or a Bedrock API key; with
        // none stored the standard AWS environment variables are used.
        api_key_env: "AWS_BEARER_TOKEN_BEDROCK",
        requires_key: false,
        signup_url: "https://console.aws.amazon.com/bedrock",
        oauth: false,
    },
    ProviderPreset {
        id: "vertex",
        label: "Google Vertex AI",
        kind: ProviderKind::Vertex,
        // Project and region decide the host; both come from the environment, the stored
        // service-account key, or an explicit base URL.
        base_url: "",
        default_model: "",
        // A service-account JSON or an access token may be stored instead.
        api_key_env: "GOOGLE_ACCESS_TOKEN",
        requires_key: false,
        signup_url: "https://console.cloud.google.com/vertex-ai",
        oauth: false,
    },
    ProviderPreset {
        id: "openai-codex",
        label: "ChatGPT / Codex subscription",
        kind: ProviderKind::Codex,
        base_url: "https://chatgpt.com/backend-api/codex",
        default_model: "gpt-5.5",
        // The credential is an OAuth grant, not a key; the env var is a manual override.
        api_key_env: "CODEX_ACCESS_TOKEN",
        requires_key: true,
        signup_url: "https://chatgpt.com",
        oauth: true,
    },
    ProviderPreset {
        id: "copilot-acp",
        label: "GitHub Copilot ACP (external agent)",
        kind: ProviderKind::Acp,
        // Not a URL: the command silver spawns. An override is stored as the base URL.
        base_url: "",
        default_model: "copilot",
        api_key_env: "",
        requires_key: false,
        signup_url: "https://github.com/github/copilot-cli",
        oauth: false,
    },
    // External agent modes: one preset per CLI, in catalog order, its default model the mode id.
    acp("claude", "Claude Code"),
    acp("cursor", "Cursor"),
    acp("pi", "Pi"),
    acp("opencode", "OpenCode"),
    acp("grok", "Grok Build"),
    acp("gemini", "Gemini CLI"),
    acp("qwen", "Qwen Code"),
    acp("goose", "goose"),
    acp("kimi", "Kimi Code"),
    acp("droid", "Factory Droid"),
    acp("amp", "Amp"),
    acp("kilo", "Kilo"),
    acp("cline", "Cline"),
    acp("auggie", "Auggie"),
    acp("vibe", "Mistral Vibe"),
    acp("kiro", "Kiro CLI"),
    acp("devin", "Devin"),
    acp("qoder", "Qoder CLI"),
    acp("codebuddy", "CodeBuddy Code"),
    acp_model("minimax-code", "MiniMax Code", "minimax"),
    acp("junie", "Junie"),
    acp("antigravity", "Google Antigravity"),
    acp("cortex", "Cortex Code"),
    acp("poolside", "Poolside"),
    ProviderPreset {
        id: "ollama",
        label: "Ollama (local)",
        kind: ProviderKind::Ollama,
        base_url: "http://localhost:11434/v1",
        default_model: "llama3.2",
        api_key_env: "",
        requires_key: false,
        signup_url: "https://ollama.com/download",
        oauth: false,
    },
    ProviderPreset {
        id: "lmstudio",
        label: "LM Studio (local)",
        kind: ProviderKind::Ollama,
        base_url: "http://localhost:1234/v1",
        default_model: "local-model",
        api_key_env: "",
        requires_key: false,
        signup_url: "https://lmstudio.ai",
        oauth: false,
    },
    ProviderPreset {
        id: "llamacpp",
        label: "llama.cpp server (local)",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "http://localhost:8080/v1",
        default_model: "",
        api_key_env: "",
        requires_key: false,
        signup_url: "https://github.com/ggml-org/llama.cpp",
        oauth: false,
    },
    ProviderPreset {
        id: "vllm",
        label: "vLLM (local)",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "http://localhost:8000/v1",
        default_model: "",
        api_key_env: "",
        requires_key: false,
        signup_url: "https://docs.vllm.ai",
        oauth: false,
    },
    ProviderPreset {
        id: "custom",
        label: "Custom OpenAI-compatible endpoint",
        kind: ProviderKind::OpenAiCompatible,
        base_url: "",
        default_model: "",
        api_key_env: "SILVER_API_KEY",
        requires_key: false,
        signup_url: "",
        oauth: false,
    },
];

pub fn preset(id: &str) -> Option<&'static ProviderPreset> {
    let needle = id.trim().to_ascii_lowercase();
    PROVIDER_PRESETS.iter().find(|p| p.id == needle)
}

/// A human name for the endpoint `base_url`, for errors and menus: the preset that serves it,
/// else "the provider" when it is a custom endpoint.
pub fn label_for_base_url(base_url: &str) -> &'static str {
    let base = base_url.trim_end_matches('/');
    PROVIDER_PRESETS
        .iter()
        .find(|p| !p.base_url.is_empty() && p.base_url.trim_end_matches('/') == base)
        .map_or("the provider", |p| p.label)
}

const LOCAL_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1", "0.0.0.0"];
/// Docker / Podman / Lima DNS names that resolve to the host machine.
const CONTAINER_LOCAL_SUFFIXES: &[&str] =
    &[".docker.internal", ".containers.internal", ".lima.internal"];

/// Whether `base_url` is this machine or its LAN: loopback, container DNS, unqualified hosts,
/// `*.local`, RFC-1918, link-local and Tailscale CGNAT, so a trusted box over Tailscale gets the
/// patience local servers need.
pub fn is_local_endpoint(base_url: &str) -> bool {
    let Some(host) = url_host(base_url) else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    if LOCAL_HOSTS.contains(&host.as_str())
        || CONTAINER_LOCAL_SUFFIXES
            .iter()
            .any(|suffix| host.ends_with(suffix))
        || host.ends_with(".local")
    {
        return true;
    }
    if let Ok(addr) = host.parse::<std::net::IpAddr>() {
        return match addr {
            std::net::IpAddr::V4(v4) => {
                v4.is_private()
                    || v4.is_loopback()
                    || v4.is_link_local()
                    // Tailscale CGNAT: 100.64.0.0/10.
                    || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1]))
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local()
            }
        };
    }
    // An unqualified hostname is local by definition; IPv6 literals have no dots either but
    // were classified by scope above, so anything with a colon left here is not local.
    !host.contains('.') && !host.contains(':')
}

/// The host of an `http(s)://` URL: userinfo, port, path and IPv6 brackets stripped.
///
/// Anything without a scheme (an ACP command, an empty override) has no host.
fn url_host(base_url: &str) -> Option<&str> {
    let rest = base_url.trim().split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or_default()
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                host
            }
            _ => authority,
        }
    };
    (!host.is_empty()).then_some(host)
}
