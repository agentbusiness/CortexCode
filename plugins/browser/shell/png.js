// @ts-check
/** PNG dimensions from the IHDR chunk (bytes 16..24 of a PNG file). */

const SIGNATURE = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

/**
 * @param {Buffer} data
 * @returns {{ width: number, height: number }}
 */
export function pngSize(data) {
  if (data.length < 24 || !data.subarray(0, 8).equals(SIGNATURE) || data.toString('ascii', 12, 16) !== 'IHDR') {
    throw new Error('not a PNG image');
  }
  return { width: data.readUInt32BE(16), height: data.readUInt32BE(20) };
}
