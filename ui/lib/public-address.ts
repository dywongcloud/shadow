import "server-only";
import net from "node:net";

const V4_BLOCKED = [
  "0.0.0.0/8",
  "10.0.0.0/8",
  "100.64.0.0/10",
  "127.0.0.0/8",
  "169.254.0.0/16",
  "172.16.0.0/12",
  "192.0.0.0/24",
  "192.0.2.0/24",
  "192.88.99.0/24",
  "192.168.0.0/16",
  "198.18.0.0/15",
  "198.51.100.0/24",
  "203.0.113.0/24",
  "224.0.0.0/4",
  "240.0.0.0/4",
].map(parseCidr);

const V6_BLOCKED_INSIDE_GLOBAL_UNICAST = ["2001::/23", "2001:db8::/32", "2002::/16", "3fff::/20"].map(parseCidr);
const V6_GLOBAL_UNICAST = parseCidr("2000::/3");
const V6_V4_MAPPED = parseCidr("::ffff:0:0/96");
const V6_NAT64 = parseCidr("64:ff9b::/96");
const INTERNAL_SUFFIXES = [".localhost", ".internal", ".local"];

function parseCidr(cidr: string): { base: Uint8Array; bits: number } {
  const [address, bits] = cidr.split("/");
  return { base: net.isIPv4(address) ? v4Bytes(address) : v6Bytes(address), bits: Number(bits) };
}

function v4Bytes(dotted: string): Uint8Array {
  return Uint8Array.from(dotted.split(".").map(Number));
}

function v6Bytes(address: string): Uint8Array {
  const unzoned = address.split("%")[0];
  const lastColon = unzoned.lastIndexOf(":");
  const tail = unzoned.slice(lastColon + 1);
  const text = tail.includes(".")
    ? `${unzoned.slice(0, lastColon + 1)}${[...v4Bytes(tail)].reduce((hex, octet, i) => hex + (i === 2 ? ":" : "") + octet.toString(16).padStart(2, "0"), "")}`
    : unzoned;
  const [head, rest = ""] = text.split("::");
  const headGroups = head ? head.split(":") : [];
  const restGroups = rest ? rest.split(":") : [];
  const fill = text.includes("::") ? 8 - headGroups.length - restGroups.length : 0;
  const groups = [...headGroups, ...Array<string>(fill).fill("0"), ...restGroups];
  const bytes = new Uint8Array(16);
  groups.forEach((group, i) => {
    const value = parseInt(group, 16);
    bytes[2 * i] = value >> 8;
    bytes[2 * i + 1] = value & 0xff;
  });
  return bytes;
}

function inPrefix(bytes: Uint8Array, { base, bits }: { base: Uint8Array; bits: number }): boolean {
  for (let i = 0; i < bits; i += 8) {
    const mask = bits - i >= 8 ? 0xff : (0xff << (8 - (bits - i))) & 0xff;
    if ((bytes[i / 8] ^ base[i / 8]) & mask) return false;
  }
  return true;
}

/** True only for a globally routable unicast address: every non-public IPv4 range, IPv6 outside 2000::/3 or inside the documentation/IETF/6to4 carve-outs, and
 *  IPv4-mapped or NAT64 forms of a non-public IPv4 are refused; anything that is not an IP literal is refused too. */
export function isPublicAddress(address: string): boolean {
  if (net.isIPv4(address)) {
    const bytes = v4Bytes(address);
    return !V4_BLOCKED.some((range) => inPrefix(bytes, range));
  }
  if (!net.isIPv6(address)) return false;
  const bytes = v6Bytes(address);
  if (inPrefix(bytes, V6_V4_MAPPED) || inPrefix(bytes, V6_NAT64)) return isPublicAddress(bytes.subarray(12).join("."));
  return inPrefix(bytes, V6_GLOBAL_UNICAST) && !V6_BLOCKED_INSIDE_GLOBAL_UNICAST.some((range) => inPrefix(bytes, range));
}

/** Why a URL hostname must never be fetched on a visitor's behalf, or null when it may be dialed (its resolved addresses are still checked at connect time). */
export function refusedHost(hostname: string): string | null {
  const host = hostname.replace(/^\[|\]$/g, "").replace(/\.$/, "").toLowerCase();
  if (net.isIP(host)) return isPublicAddress(host) ? null : "target is a non-public address";
  if (!host.includes(".") || INTERNAL_SUFFIXES.some((suffix) => host.endsWith(suffix))) return "target is an internal hostname";
  return null;
}
