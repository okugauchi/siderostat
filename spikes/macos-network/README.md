# macOS network API spike

This read-only spike validates the public APIs required by section 13 of the Siderostat specification maintained in the Obsidian Vault at `Projects/siderostat/docs/spec.md`. It does not change interface, service, address, or route configuration. The optional Bonjour probe registers an ephemeral `_ds4cluster._tcp` service and removes it before exit.

## Build

```sh
clang -Wall -Wextra -Werror -std=c17 network_probe.c \
  -framework CoreFoundation \
  -framework SystemConfiguration \
  -o network-probe
```

## Commands

```sh
./network-probe snapshot
./network-probe watch 30
./network-probe bonjour
```

- `snapshot` uses `SCNetworkServiceGetEnabled`, `SCDynamicStoreCopyValue`, `if_nametoindex`, and `getifaddrs`.
- `watch` subscribes to `State:/Network/Interface/bridge0/...` and `Setup:/Network/Service/...` through `SCDynamicStoreSetNotificationKeys`. It reports key count only; it does not dump machine configuration.
- `bonjour` passes the `bridge0` interface index to both `DNSServiceRegister` and `DNSServiceBrowse`, uses `htons` for the service port, processes callbacks, and deallocates both `DNSServiceRef` values.

## Binding decision

Production Rust code will use `system-configuration` 0.7 as the single System Configuration binding dependency. It provides the high-level `SCDynamicStore` surface and re-exports `system-configuration-sys` for missing notification/configuration calls. POSIX `if_nametoindex`/`getifaddrs` and DNS-SD remain small platform FFI wrappers because they are not System Configuration APIs.

Not selected:

- Shell parsing (`networksetup`, `ifconfig`, `scutil`): unsuitable for correctness and localization-sensitive.
- Private IOKit driver classes: not a stable correctness contract.
- A handwritten binding for all Core Foundation/System Configuration APIs: unnecessary unsafe surface compared with the maintained crate.

The C probe is kept only as an API/linkage fixture. Production implementation must wrap ownership, callback lifetime, cancellation, and generation binding in Rust.
