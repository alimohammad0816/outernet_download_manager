use serde::Serialize;
use std::{collections::BTreeMap, net::IpAddr};

#[derive(Serialize)]
pub struct NetworkInterface {
    pub name: String,
    pub address: IpAddr,
}

#[derive(Clone)]
pub struct Binding {
    pub name: String,
    pub address: IpAddr,
}

pub fn interfaces() -> Result<Vec<NetworkInterface>, String> {
    let adapters = if_addrs::get_if_addrs()
        .map_err(|error| format!("Cannot list network interfaces: {error}"))?;
    let mut addresses = BTreeMap::<String, IpAddr>::new();
    for adapter in adapters {
        if !matches!(
            adapter.oper_status,
            if_addrs::IfOperStatus::Up | if_addrs::IfOperStatus::Unknown
        ) {
            continue;
        }
        let address = adapter.ip();
        if address.is_loopback()
            || address.is_unspecified()
            || address.is_multicast()
            || matches!(address, IpAddr::V6(address) if address.is_unicast_link_local())
            || matches!(address, IpAddr::V4(address) if address.is_link_local())
        {
            continue;
        }
        let existing = addresses.entry(adapter.name).or_insert(address);
        if address.is_ipv4() && existing.is_ipv6() {
            *existing = address;
        }
    }
    Ok(addresses
        .into_iter()
        .map(|(name, address)| NetworkInterface { name, address })
        .collect())
}

pub fn bind(name: &str) -> Result<Binding, String> {
    interfaces()?
        .into_iter()
        .find(|interface| interface.name == name)
        .map(|interface| Binding {
            name: interface.name,
            address: interface.address,
        })
        .ok_or_else(|| format!("Network interface {name} is unavailable. Reconnect it and retry."))
}
