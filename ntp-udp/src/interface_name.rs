//! taken from https://docs.rs/nix/latest/src/nix/ifaddrs.rs.html
//! stripped to just the parts that we need.
//!
//! Query network interface addresses
//!
//! Uses the Linux and/or BSD specific function `getifaddrs` to query the list
//! of interfaces and their associated addresses.

use std::ffi;
use std::iter::Iterator;
use std::mem;
use std::net::SocketAddr;
use std::option::Option;

#[allow(dead_code)]
pub fn interface_name(local_addr: SocketAddr) -> std::io::Result<Option<[u8; 16]>> {
    let matches_inferface = |interface: &InterfaceAddress| match interface.address {
        None => false,
        Some(address) => address.ip() == local_addr.ip(),
    };

    if let Some(interface) = getifaddrs()?.find(matches_inferface) {
        let mut ifrn_name = [0; 16];

        let name = interface.interface_name;
        let length = Ord::min(name.len(), ifrn_name.len());
        ifrn_name[0..length].copy_from_slice(&name.as_bytes()[0..length]);

        Ok(Some(ifrn_name))
    } else {
        Ok(None)
    }
}

/// Describes a single address for an interface as returned by `getifaddrs`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct InterfaceAddress {
    /// Name of the network interface
    interface_name: String,
    /// Network address of this interface
    address: Option<SocketAddr>,
}

impl InterfaceAddress {
    /// Create an `InterfaceAddress` from the libc struct.
    ///
    /// # Safety
    ///
    /// assumes a valid `libc::ifaddrs`
    unsafe fn from_libc_ifaddrs(info: &libc::ifaddrs) -> InterfaceAddress {
        let ifname = ffi::CStr::from_ptr(info.ifa_name);

        let sockaddr: *mut libc::sockaddr = info.ifa_addr;
        let address = Self::to_socket_addr(sockaddr);

        let addr = InterfaceAddress {
            interface_name: ifname.to_string_lossy().to_string(),
            address,
        };

        addr
    }

    /// Convert a libc::sockaddr to a rust std::net::SocketAddr
    ///
    /// # Safety
    ///
    /// assumes a valid sockaddr
    unsafe fn to_socket_addr(sockaddr: *const libc::sockaddr) -> Option<SocketAddr> {
        match (*sockaddr).sa_family as libc::c_int {
            libc::AF_INET => {
                let inaddr: libc::sockaddr_in = *(sockaddr as *const libc::sockaddr_in);

                let socketaddr = std::net::SocketAddrV4::new(
                    std::net::Ipv4Addr::from(inaddr.sin_addr.s_addr.to_le_bytes()),
                    inaddr.sin_port,
                );

                Some(std::net::SocketAddr::V4(socketaddr))
            }
            libc::AF_INET6 => {
                let inaddr: libc::sockaddr_in6 = *(sockaddr as *const libc::sockaddr_in6);

                let sin_addr = inaddr.sin6_addr.s6_addr;
                let segment_bytes: [u8; 16] =
                    std::ptr::read_unaligned(&sin_addr as *const _ as *const _);

                let socketaddr = std::net::SocketAddrV6::new(
                    std::net::Ipv6Addr::from(segment_bytes),
                    inaddr.sin6_port,
                    inaddr.sin6_flowinfo,
                    inaddr.sin6_scope_id,
                );

                Some(std::net::SocketAddr::V6(socketaddr))
            }
            _ => None,
        }
    }
}

/// Holds the results of `getifaddrs`.
///
/// Use the function `getifaddrs` to create this Iterator. Note that the
/// actual list of interfaces can be iterated once and will be freed as
/// soon as the Iterator goes out of scope.
#[derive(Debug, Eq, Hash, PartialEq)]
struct InterfaceAddressIterator {
    base: *mut libc::ifaddrs,
    next: *mut libc::ifaddrs,
}

impl Drop for InterfaceAddressIterator {
    fn drop(&mut self) {
        unsafe { libc::freeifaddrs(self.base) };
    }
}

impl Iterator for InterfaceAddressIterator {
    type Item = InterfaceAddress;
    fn next(&mut self) -> Option<<Self as Iterator>::Item> {
        match unsafe { self.next.as_ref() } {
            Some(ifaddr) => {
                self.next = ifaddr.ifa_next;
                // SAFETY: assumes the ifaddr is valid
                Some(unsafe { InterfaceAddress::from_libc_ifaddrs(ifaddr) })
            }
            None => None,
        }
    }
}

/// Get interface addresses using libc's `getifaddrs`
fn getifaddrs() -> std::io::Result<InterfaceAddressIterator> {
    let mut addrs = mem::MaybeUninit::<*mut libc::ifaddrs>::uninit();

    crate::cerr(unsafe { libc::getifaddrs(addrs.as_mut_ptr()) })?;

    Ok(InterfaceAddressIterator {
        base: unsafe { addrs.assume_init() },
        next: unsafe { addrs.assume_init() },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_interface() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:8014").unwrap();
        let name = interface_name(socket.local_addr().unwrap()).unwrap();

        assert!(name.is_some());
    }

    #[test]
    fn find_interface_ipv6() {
        let socket = std::net::UdpSocket::bind("::1:8015").unwrap();
        let name = interface_name(socket.local_addr().unwrap()).unwrap();

        assert!(name.is_some());
    }
}