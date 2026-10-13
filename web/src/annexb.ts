// Annex B helpers: NAL splitting and rebuilding.

// Splits an Annex B access unit into NAL units (without start codes).
export function nalUnits(bytes: Uint8Array): Uint8Array[] {
  const out: Uint8Array[] = [];
  let start = -1;
  for (let i = 0; i + 3 <= bytes.length; i += 1) {
    if (bytes[i] === 0 && bytes[i + 1] === 0 && bytes[i + 2] === 1) {
      if (start >= 0) {
        let end = i;
        if (end > start && bytes[end - 1] === 0) end -= 1;
        out.push(bytes.subarray(start, end));
      }
      start = i + 3;
      i += 2;
    }
  }
  if (start >= 0) out.push(bytes.subarray(start));
  return out;
}

export function joinNalUnits(nals: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(nals.reduce((n, nal) => n + 4 + nal.length, 0));
  let offset = 0;
  for (const nal of nals) {
    out.set([0, 0, 0, 1], offset);
    out.set(nal, offset + 4);
    offset += 4 + nal.length;
  }
  return out;
}

export function sameBytes(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i += 1) if (a[i] !== b[i]) return false;
  return true;
}
