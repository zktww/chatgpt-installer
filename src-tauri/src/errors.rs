//! 统一错误类型：错误码 + 中文说明 + 修复建议，前端可直接展示。
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AppError {
    pub code: String,
    pub message: String,
    pub hint: String,
}

impl AppError {
    pub fn new(code: &str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            hint: hint.into(),
        }
    }

    pub fn need_elevation() -> Self {
        Self::new(
            "NEED_ELEVATION",
            "安装需要管理员权限",
            "点击确定后程序将请求提权并自动继续安装，这是安装应用包的常规要求。",
        )
    }

    pub fn cancelled() -> Self {
        Self::new(
            "CANCELLED",
            "已取消",
            "下载已停止，进度已保留，随时可以继续。",
        )
    }

    pub fn paused() -> Self {
        Self::new(
            "PAUSED",
            "已暂停",
            "下载已暂停，进度已保留，点击继续可恢复下载。",
        )
    }

    /// 把 PowerShell / 部署工具的原始输出映射到已知错误码。
    pub fn from_deploy_output(output: &str) -> Self {
        // 注意：先转大写再匹配，模式串必须也是大写（"0x" 会变成 "0X"）
        let o = output.to_uppercase();
        if o.contains("0X80073D02") {
            Self::new(
                "0x80073D02",
                "ChatGPT 正在运行中，无法覆盖安装",
                "请完全退出程序（含右下角托盘图标）后重试；或在任务管理器中结束 ChatGPT.exe。",
            )
        } else if o.contains("0X80073CFD") {
            Self::new(
                "0x80073CFD",
                "系统禁止安装本地应用包（旁加载被禁用）",
                "请以管理员身份重新运行本程序，程序会自动放行；或手动到「设置 → 系统 → 开发者选项」开启旁加载。",
            )
        } else if o.contains("0X800B0109") {
            Self::new(
                "0x800B0109",
                "安装包签名不受信任",
                "安装包可能在传输中损坏。请删除安装包后重新下载；若反复出现请更换网络环境。",
            )
        } else if o.contains("0X80073D28") {
            Self::new(
                "0x80073D28",
                "安装需要管理员权限",
                "请以管理员身份运行本程序后重试。",
            )
        } else if o.contains("0X80070005") {
            Self::new(
                "0x80070005",
                "拒绝访问",
                "可能是权限不足或安全软件拦截。请以管理员身份运行，或暂时关闭拦截后重试。",
            )
        } else if o.contains("0X80073CF3") {
            Self::new(
                "0x80073CF3",
                "安装包依赖缺失或冲突",
                "请先通过 Windows 更新把系统补到最新，再重试安装。",
            )
        } else if o.contains("0X80073D06") {
            Self::new(
                "0x80073D06",
                "已安装更新版本的 ChatGPT",
                "当前安装包版本低于已安装版本，无需重复安装。",
            )
        } else {
            // 未知错误：保留原始输出片段，方便排错
            let snippet: String = output.chars().take(400).collect();
            Self::new(
                "INSTALL_FAILED",
                "安装失败（未知错误）",
                format!(
                    "请点击「复制诊断日志」并附上以下信息反馈：{}",
                    snippet.trim()
                ),
            )
        }
    }

    pub fn network(detail: impl Into<String>) -> Self {
        Self::new(
            "NETWORK_ERROR",
            "网络连接失败",
            format!(
                "无法连接到官方下载源（{}）。请检查网络/代理后重试，或尝试「换源」。",
                detail.into()
            ),
        )
    }

    /// 该错误是否暗示“换管理员身份重试可能成功”。
    /// 注意：已提权仍报此类错误时，应直接返回原始错误，避免提权死循环。
    pub fn is_elevation_error(&self) -> bool {
        matches!(self.code.as_str(), "0x80073D28" | "0x80070005")
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}
