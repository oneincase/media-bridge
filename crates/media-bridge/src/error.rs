//! 错误类型 —— 每种失败都要能被消费端**分门别类地处理**（是没装依赖？没授权？
//! 还是播放器不支持这个命令？），因为它们的用户提示完全不同。

/// 中间件错误。
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// 当前平台压根没有这条路径（例如 Windows 上的 `kAudioTapPropertyUID`）
    #[error("当前平台不支持：{0}")]
    Unsupported(String),
    /// 环境缺东西：没装依赖、没有可用的数据源
    #[error("数据源不可用：{0}")]
    Unavailable(String),
    /// 用户拒绝了权限
    #[error("权限被拒绝：{0}")]
    Denied(String),
    /// 播放器不支持该操作（能力位为 false，或播放器明确拒绝）
    #[error("当前播放器不支持：{0}")]
    NotSupported(String),
    /// 没有正在播放的媒体
    #[error("当前没有正在播放的媒体")]
    NoMedia,
    /// 协议层错误（请求格式不对、未知方法）
    #[error("协议错误：{0}")]
    Protocol(String),
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl BridgeError {
    /// wire 错误码（消费端 switch 用，不随文案变）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unsupported(_) => "unsupported",
            Self::Unavailable(_) => "unavailable",
            Self::Denied(_) => "denied",
            Self::NotSupported(_) => "not-supported",
            Self::NoMedia => "no-media",
            Self::Protocol(_) => "protocol",
            Self::Io(_) => "io",
            Self::Other(_) => "other",
        }
    }

    /// 这个错误是否代表「环境问题，用户能做点什么」——
    /// 只有这类才值得在 UI 里弹提示，其余归为「此次失败，稍后重试」。
    pub fn is_user_actionable(&self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::Denied(_) | Self::Unsupported(_))
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::Unavailable(msg.into())
    }

    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }
}

pub type Result<T, E = BridgeError> = std::result::Result<T, E>;
