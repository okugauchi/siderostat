#include <CoreFoundation/CoreFoundation.h>
#include <SystemConfiguration/SystemConfiguration.h>
#include <arpa/inet.h>
#include <dns_sd.h>
#include <errno.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/socket.h>

static const char *const kBridgeName = "bridge0";
static const char *const kServiceType = "_ds4cluster._tcp";

static unsigned prefix_bits(const struct sockaddr_in *mask) {
    uint32_t bits = ntohl(mask->sin_addr.s_addr);
    unsigned count = 0;
    while ((bits & 0x80000000U) != 0U) {
        ++count;
        bits <<= 1U;
    }
    return count;
}

static bool bridge_service_enabled(void) {
    bool found = false;
    bool enabled = false;
    SCPreferencesRef preferences =
        SCPreferencesCreate(NULL, CFSTR("ds4-smart-proxy-network-spike"), NULL);
    if (preferences == NULL) {
        fprintf(stderr, "SCPreferencesCreate failed: %s\n", SCErrorString(SCError()));
        return false;
    }

    CFArrayRef services = SCNetworkServiceCopyAll(preferences);
    if (services != NULL) {
        const CFIndex count = CFArrayGetCount(services);
        for (CFIndex index = 0; index < count; ++index) {
            SCNetworkServiceRef service =
                (SCNetworkServiceRef)CFArrayGetValueAtIndex(services, index);
            SCNetworkInterfaceRef interface = SCNetworkServiceGetInterface(service);
            CFStringRef bsd_name =
                interface == NULL ? NULL : SCNetworkInterfaceGetBSDName(interface);
            if (bsd_name != NULL &&
                CFStringCompare(bsd_name, CFSTR("bridge0"), 0) == kCFCompareEqualTo) {
                found = true;
                enabled = SCNetworkServiceGetEnabled(service);
                break;
            }
        }
        CFRelease(services);
    }
    CFRelease(preferences);
    printf("service_found=%s service_enabled=%s\n", found ? "true" : "false",
           enabled ? "true" : "false");
    return found && enabled;
}

static void dynamic_store_snapshot(void) {
    SCDynamicStoreRef store =
        SCDynamicStoreCreate(NULL, CFSTR("ds4-smart-proxy-network-spike"), NULL, NULL);
    if (store == NULL) {
        fprintf(stderr, "SCDynamicStoreCreate failed: %s\n", SCErrorString(SCError()));
        return;
    }
    CFPropertyListRef link = SCDynamicStoreCopyValue(
        store, CFSTR("State:/Network/Interface/bridge0/Link"));
    CFPropertyListRef ipv4 = SCDynamicStoreCopyValue(
        store, CFSTR("State:/Network/Interface/bridge0/IPv4"));
    printf("dynamic_store_link=%s dynamic_store_ipv4=%s\n",
           link == NULL ? "absent" : "present", ipv4 == NULL ? "absent" : "present");
    if (link != NULL) {
        CFRelease(link);
    }
    if (ipv4 != NULL) {
        CFRelease(ipv4);
    }
    CFRelease(store);
}

static int snapshot(void) {
    const unsigned interface_index = if_nametoindex(kBridgeName);
    printf("interface_index=%u\n", interface_index);
    (void)bridge_service_enabled();
    dynamic_store_snapshot();

    struct ifaddrs *addresses = NULL;
    if (getifaddrs(&addresses) != 0) {
        fprintf(stderr, "getifaddrs failed: %s\n", strerror(errno));
        return EXIT_FAILURE;
    }
    bool found = false;
    for (const struct ifaddrs *item = addresses; item != NULL; item = item->ifa_next) {
        if (item->ifa_addr == NULL || item->ifa_netmask == NULL ||
            strcmp(item->ifa_name, kBridgeName) != 0 ||
            item->ifa_addr->sa_family != AF_INET) {
            continue;
        }
        const struct sockaddr_in *mask = (const struct sockaddr_in *)item->ifa_netmask;
        printf("getifaddrs_ipv4=true up=%s prefix_bits=%u\n",
               (item->ifa_flags & IFF_UP) != 0 ? "true" : "false", prefix_bits(mask));
        found = true;
    }
    if (!found) {
        puts("getifaddrs_ipv4=false");
    }
    freeifaddrs(addresses);
    return EXIT_SUCCESS;
}

static void store_changed(SCDynamicStoreRef store, CFArrayRef changed_keys, void *context) {
    (void)store;
    (void)context;
    printf("dynamic_store_event key_count=%ld\n", (long)CFArrayGetCount(changed_keys));
    fflush(stdout);
}

static int watch(unsigned seconds) {
    SCDynamicStoreContext context = {0, NULL, NULL, NULL, NULL};
    SCDynamicStoreRef store = SCDynamicStoreCreate(
        NULL, CFSTR("ds4-smart-proxy-network-spike"), store_changed, &context);
    if (store == NULL) {
        fprintf(stderr, "SCDynamicStoreCreate failed: %s\n", SCErrorString(SCError()));
        return EXIT_FAILURE;
    }

    const void *patterns_values[] = {
        CFSTR("State:/Network/Interface/bridge0/.*"),
        CFSTR("Setup:/Network/Service/.*/.*"),
    };
    CFArrayRef patterns = CFArrayCreate(NULL, patterns_values, 2, &kCFTypeArrayCallBacks);
    if (patterns == NULL || !SCDynamicStoreSetNotificationKeys(store, NULL, patterns)) {
        fprintf(stderr, "SCDynamicStoreSetNotificationKeys failed: %s\n",
                SCErrorString(SCError()));
        if (patterns != NULL) {
            CFRelease(patterns);
        }
        CFRelease(store);
        return EXIT_FAILURE;
    }

    CFRunLoopSourceRef source = SCDynamicStoreCreateRunLoopSource(NULL, store, 0);
    if (source == NULL) {
        fprintf(stderr, "SCDynamicStoreCreateRunLoopSource failed: %s\n",
                SCErrorString(SCError()));
        CFRelease(patterns);
        CFRelease(store);
        return EXIT_FAILURE;
    }
    CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
    (void)CFRunLoopRunInMode(kCFRunLoopDefaultMode, (CFTimeInterval)seconds, false);
    CFRunLoopRemoveSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
    CFRunLoopSourceInvalidate(source);
    CFRelease(source);
    CFRelease(patterns);
    CFRelease(store);
    return EXIT_SUCCESS;
}

static void register_reply(DNSServiceRef service, DNSServiceFlags flags,
                           DNSServiceErrorType error, const char *name,
                           const char *type, const char *domain, void *context) {
    (void)service;
    (void)flags;
    (void)name;
    (void)type;
    (void)domain;
    (void)context;
    printf("bonjour_register_callback error=%d\n", error);
}

static void browse_reply(DNSServiceRef service, DNSServiceFlags flags,
                         uint32_t interface_index, DNSServiceErrorType error,
                         const char *name, const char *type, const char *domain,
                         void *context) {
    (void)service;
    (void)flags;
    (void)name;
    (void)type;
    (void)domain;
    (void)context;
    printf("bonjour_browse_callback interface_index=%u error=%d\n", interface_index, error);
}

static int process_dns_service(DNSServiceRef service, unsigned timeout_seconds) {
    const int descriptor = DNSServiceRefSockFD(service);
    if (descriptor < 0) {
        return EXIT_FAILURE;
    }
    fd_set read_set;
    FD_ZERO(&read_set);
    FD_SET(descriptor, &read_set);
    struct timeval timeout = {(time_t)timeout_seconds, 0};
    const int ready = select(descriptor + 1, &read_set, NULL, NULL, &timeout);
    if (ready < 0) {
        return EXIT_FAILURE;
    }
    if (ready == 0) {
        return EXIT_SUCCESS;
    }
    return DNSServiceProcessResult(service) == kDNSServiceErr_NoError ? EXIT_SUCCESS
                                                                      : EXIT_FAILURE;
}

static int bonjour(void) {
    const uint32_t interface_index = if_nametoindex(kBridgeName);
    if (interface_index == 0) {
        fprintf(stderr, "bridge0 has no interface index; Bonjour probe skipped\n");
        return EXIT_FAILURE;
    }
    DNSServiceRef registration = NULL;
    DNSServiceRef browser = NULL;
    DNSServiceErrorType error = DNSServiceRegister(
        &registration, 0, interface_index, "ds4-network-spike", kServiceType, NULL,
        NULL, htons(9920), 0, NULL, register_reply, NULL);
    if (error != kDNSServiceErr_NoError) {
        fprintf(stderr, "DNSServiceRegister failed: %d\n", error);
        return EXIT_FAILURE;
    }
    error = DNSServiceBrowse(&browser, 0, interface_index, kServiceType, NULL,
                             browse_reply, NULL);
    if (error != kDNSServiceErr_NoError) {
        fprintf(stderr, "DNSServiceBrowse failed: %d\n", error);
        DNSServiceRefDeallocate(registration);
        return EXIT_FAILURE;
    }

    const int register_result = process_dns_service(registration, 2);
    const int browse_result = process_dns_service(browser, 2);
    DNSServiceRefDeallocate(browser);
    DNSServiceRefDeallocate(registration);
    return register_result == EXIT_SUCCESS && browse_result == EXIT_SUCCESS ? EXIT_SUCCESS
                                                                            : EXIT_FAILURE;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s snapshot|watch [seconds]|bonjour\n", argv[0]);
        return EXIT_FAILURE;
    }
    if (strcmp(argv[1], "snapshot") == 0) {
        return snapshot();
    }
    if (strcmp(argv[1], "watch") == 0) {
        const unsigned seconds = argc >= 3 ? (unsigned)strtoul(argv[2], NULL, 10) : 5U;
        return watch(seconds == 0 ? 5U : seconds);
    }
    if (strcmp(argv[1], "bonjour") == 0) {
        return bonjour();
    }
    fprintf(stderr, "unknown command: %s\n", argv[1]);
    return EXIT_FAILURE;
}
