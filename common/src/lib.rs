//! 跨二进制共享的应用层工具：日志、时间戳、摘要、体积展示。
//!
//! 码制相关实现只在 `screencode`，这里不引入任何协议知识。

pub mod clock;
pub mod digest;
pub mod format;
pub mod logging;
