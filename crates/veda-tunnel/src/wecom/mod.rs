//! WeCom (企业微信) aibot adapter — the first tunnel channel.
//!
//! Future IMs (feishu / dingtalk) become sibling modules; `config`,
//! `registry`, `veda`, and `admin` stay generic.

pub mod conn;
pub mod handler;
pub mod protocol;
