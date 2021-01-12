use namib_shared::config_firewall::{EnTarget, FirewallConfig, FirewallRule, NetworkHost, Protocol};

use crate::{error::Result, models::model_firewall::FirewallConfigState, services::is_system_mode, uci::UCI};
use nftnl::{nft_expr, Batch, Chain, FinalizedBatch, ProtoFamily, Rule, Table};
use std::{ffi::CString, net::IpAddr};

/// This file represent the service for firewall on openwrt.
///
/// Created on 11.11.2020.
///
/// @author Namib Group 3.

/// The folder where the configuration file should be stored.
const CONFIG_DIR: &str = "config";
const SAVE_DIR: &str = "/tmp/.uci_namib";
const TABLE_NAME: &str = "namib";
const BASE_CHAIN_NAME: &str = "base_chain";

pub fn handle_new_config(firewall_state: &FirewallConfigState, config: FirewallConfig) -> Result<()> {
    let mut batch = Batch::new();
    add_old_config_deletion_instructions(&mut batch);
    convert_config_to_nftnl_commands(&mut batch, &config)?;
    let batch = batch.finalize();
    // TODO proper error handling
    send_and_process(&batch).unwrap();

    Ok(())
}

fn add_old_config_deletion_instructions(batch: &mut Batch) -> Result<()> {
    // TODO
    Ok(())
}

fn convert_config_to_nftnl_commands(batch: &mut Batch, config: &FirewallConfig) -> Result<()> {
    let table = Table::new(&CString::new(TABLE_NAME).unwrap(), ProtoFamily::Inet);
    batch.add(&table, nftnl::MsgType::Add);

    let mut base_chain = Chain::new(&CString::new(BASE_CHAIN_NAME).unwrap(), &table);
    base_chain.set_hook(nftnl::Hook::In, 0);
    base_chain.set_policy(nftnl::Policy::Accept);
    batch.add(&base_chain, nftnl::MsgType::Add);

    for device in config.devices() {
        let mut device_chain = Chain::new(&CString::new(device.id.to_string()).unwrap(), &table);
        // Drop packets if they are not explicitly allowed for a device.
        device_chain.set_policy(nftnl::Policy::Drop);
        batch.add(&device_chain, nftnl::MsgType::Add);

        let mut device_jump_rule_src = Rule::new(&base_chain);
        device_jump_rule_src.add_expr(&nft_expr!(meta nfproto));
        let mut device_jump_rule_dst = Rule::new(&base_chain);
        device_jump_rule_dst.add_expr(&nft_expr!(meta nfproto));
        match device.ip {
            IpAddr::V4(v4addr) => {
                // TODO the following three lines are repeated multiple times with different variations in the function,
                //      they should be moved to a separate function instead.
                device_jump_rule_src.add_expr(&nft_expr!(cmp == libc::NFPROTO_IPV4 as u8));
                device_jump_rule_src.add_expr(&nft_expr!(payload ipv4 saddr));
                device_jump_rule_src.add_expr(&nft_expr!(cmp == v4addr));
                device_jump_rule_dst.add_expr(&nft_expr!(cmp == libc::NFPROTO_IPV4 as u8));
                device_jump_rule_dst.add_expr(&nft_expr!(payload ipv4 daddr));
                device_jump_rule_dst.add_expr(&nft_expr!(cmp == v4addr));
            },
            IpAddr::V6(v6addr) => {
                device_jump_rule_src.add_expr(&nft_expr!(cmp == libc::NFPROTO_IPV6 as u8));
                device_jump_rule_src.add_expr(&nft_expr!(payload ipv6 saddr));
                device_jump_rule_src.add_expr(&nft_expr!(cmp == v6addr));
                device_jump_rule_dst.add_expr(&nft_expr!(cmp == libc::NFPROTO_IPV6 as u8));
                device_jump_rule_dst.add_expr(&nft_expr!(payload ipv6 daddr));
                device_jump_rule_dst.add_expr(&nft_expr!(cmp == v6addr));
            },
        }
        device_jump_rule_src.add_expr(&nft_expr!(verdict jump CString::new(device.id.to_string()).unwrap()));
        device_jump_rule_dst.add_expr(&nft_expr!(verdict jump CString::new(device.id.to_string()).unwrap()));
        batch.add(&device_jump_rule_src, nftnl::MsgType::Add);
        batch.add(&device_jump_rule_dst, nftnl::MsgType::Add);

        for rule_spec in &device.rules {
            let mut current_rule = Rule::new(&device_chain);
            // TODO handling of DNS names.
            if let Some(NetworkHost::Ip(ipaddr)) = rule_spec.src.host {
                match ipaddr {
                    IpAddr::V4(v4addr) => {
                        current_rule.add_expr(&nft_expr!(cmp == libc::NFPROTO_IPV4 as u8));
                        current_rule.add_expr(&nft_expr!(payload ipv4 saddr));
                        current_rule.add_expr(&nft_expr!(cmp == v4addr));
                    },
                    IpAddr::V6(v6addr) => {
                        current_rule.add_expr(&nft_expr!(cmp == libc::NFPROTO_IPV6 as u8));
                        current_rule.add_expr(&nft_expr!(payload ipv6 saddr));
                        current_rule.add_expr(&nft_expr!(cmp == v6addr));
                    },
                }
            }
            if let Some(NetworkHost::Ip(ipaddr)) = rule_spec.dst.host {
                match ipaddr {
                    IpAddr::V4(v4addr) => {
                        current_rule.add_expr(&nft_expr!(cmp == libc::NFPROTO_IPV4 as u8));
                        current_rule.add_expr(&nft_expr!(payload ipv4 daddr));
                        current_rule.add_expr(&nft_expr!(cmp == v4addr));
                        match rule_spec.protocol {
                            Protocol::Tcp => {
                                current_rule.add_expr(&nft_expr!(payload ipv4 protocol));
                                current_rule.add_expr(&nft_expr!(cmp == "tcp"));
                            },
                            Protocol::Udp => {
                                current_rule.add_expr(&nft_expr!(payload ipv4 protocol));
                                current_rule.add_expr(&nft_expr!(cmp == "udp"));
                            },
                            _ => {}, // TODO expand with further options (icmp, sctp)
                        }
                    },
                    IpAddr::V6(v6addr) => {
                        current_rule.add_expr(&nft_expr!(cmp == libc::NFPROTO_IPV6 as u8));
                        current_rule.add_expr(&nft_expr!(payload ipv6 daddr));
                        current_rule.add_expr(&nft_expr!(cmp == v6addr));
                        // TODO support for protocol match in IPv6 (needs to be added in nftnl library)
                    },
                }
            }
            match rule_spec.target {
                EnTarget::ACCEPT => current_rule.add_expr(&nft_expr!(verdict accept)),
                EnTarget::REJECT => {
                    current_rule.add_expr(&nft_expr!(verdict drop))
                    //current_rule.add_expr(&nft_expr!(verdict reject tcp-rst))
                    // TODO use ICMP reject
                },
                EnTarget::DROP => current_rule.add_expr(&nft_expr!(verdict drop)),
            }
            batch.add(&current_rule, nftnl::MsgType::Add);
        }
    }

    Ok(())
}

/// Taken from https://github.com/mullvad/nftnl-rs/blob/master/nftnl/examples/add-rules.rs
fn send_and_process(batch: &FinalizedBatch) -> Result<()> {
    // Create a netlink socket to netfilter.
    let socket = mnl::Socket::new(mnl::Bus::Netfilter)?;
    // Send all the bytes in the batch.
    socket.send_all(batch)?;

    // Try to parse the messages coming back from netfilter. This part is still very unclear.
    let portid = socket.portid();
    let mut buffer = vec![0; nftnl::nft_nlmsg_maxsize() as usize];
    let very_unclear_what_this_is_for = 2;
    while let Some(message) = socket_recv(&socket, &mut buffer[..])? {
        match mnl::cb_run(message, very_unclear_what_this_is_for, portid)? {
            mnl::CbResult::Stop => {
                break;
            },
            mnl::CbResult::Ok => (),
        }
    }
    Ok(())
}

/// Taken from https://github.com/mullvad/nftnl-rs/blob/master/nftnl/examples/add-rules.rs
fn socket_recv<'a>(socket: &mnl::Socket, buf: &'a mut [u8]) -> Result<Option<&'a [u8]>> {
    let ret = socket.recv(buf)?;
    if ret > 0 {
        Ok(Some(&buf[..ret]))
    } else {
        Ok(None)
    }
}
