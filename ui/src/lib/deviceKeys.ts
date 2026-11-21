// REQ: FLT-005 (GitHub issue #73) — can a configured device be the one at an address? Read from the
// device's own `match` keys (what `GET /api/v1/clients` lists), so a name that merely equals what the
// query log shows for an address doesn't count as that address's device.
import type { S } from './api';

const MAC = /^[0-9a-f]{2}([:-][0-9a-f]{2}){5}$/i;

/** `a.b.c.d` as a number, or null (an IPv6 address, or not an address). */
function v4(s: string): number | null {
  const p = s.split('.');
  if (p.length !== 4) return null;
  let n = 0;
  for (const x of p) {
    if (!/^\d{1,3}$/.test(x) || Number(x) > 255) return null;
    n = n * 256 + Number(x);
  }
  return n;
}

/** Whether `address` is in the IPv4 network `cidr`; false for anything it can't tell (IPv6 networks too). */
function inNetwork(cidr: string, address: string): boolean {
  const [net, bits] = cidr.split('/');
  const a = v4(address);
  const n = v4(net);
  const len = Number(bits);
  if (a === null || n === null || !Number.isInteger(len) || len < 0 || len > 32) return false;
  const mask = len === 0 ? 0 : (0xffffffff << (32 - len)) >>> 0;
  return (((a ^ n) & mask) >>> 0) === 0;
}

/**
 * Could this configured device be the one at `address`? A MAC address or a client ID can (the node
 * that answered may have seen it, this one can't); an IP or a network must hold the address. When
 * unsure the answer is no: the address itself is always something a rule can name.
 */
export function mayBeAt(device: S['ClientInfo'], address: string): boolean {
  return device.match.some((key) => {
    if (key.startsWith('id:') || MAC.test(key)) return true;
    return key.includes('/') ? inNetwork(key, address) : key.toLowerCase() === address.toLowerCase();
  });
}
