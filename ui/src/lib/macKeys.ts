// REQ: API-010, FLT-006 (ADR-183) — a device's `match` keys: MAC addresses and addresses, compared
// the way the server compares them (a MAC as bytes, whatever its spelling; an address with its
// /32 or /128 and its IPv4-mapped form).

const MAC = /^[0-9a-f]{2}([:-][0-9a-f]{2}){5}$/i;

/** Whether `s` is a MAC address (`aa:bb:cc:dd:ee:ff` or `aa-bb-cc-dd-ee-ff`). */
export const isMac = (s: string) => MAC.test(s.trim());

/** A MAC address in one form (lowercase, colons), so two spellings of one compare equal. */
export const normMac = (m: string) => m.trim().toLowerCase().replace(/-/g, ':');

/** The address a host key stands for: `a.b.c.d/32` and `x::y/128` are hosts, `::ffff:a.b.c.d` is `a.b.c.d`. */
function host(s: string): string {
  let h = s.trim().toLowerCase();
  if (h.includes(':') && h.endsWith('/128')) h = h.slice(0, -4);
  else if (!h.includes(':') && h.endsWith('/32')) h = h.slice(0, -3);
  return h.startsWith('::ffff:') && h.includes('.') ? h.slice(7) : h;
}

/** Whether the key names exactly the address `ip` (and no network, no MAC, no client ID). */
export function sameIp(key: string, ip: string): boolean {
  return host(key) === host(ip);
}

/** Whether a device's keys already list the MAC address `mac`. */
export const listsMac = (match: string[], mac: string) => match.some((k) => isMac(k) && normMac(k) === normMac(mac));
