#[allow(clippy::module_inception)]
#[path = "./reconnect/reconnect.rs"]
mod reconnect;

#[path = "./reconnect/same_ip.rs"]
mod same_ip;

#[path = "./reconnect/timeout.rs"]
mod timeout;
