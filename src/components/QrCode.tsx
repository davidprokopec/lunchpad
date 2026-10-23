import { encode } from "uqr";
import { useMemo } from "react";

/** SVG path of the dark modules of a QR code, one unit per module (quiet zone included). */
export function qrPath(text: string, border = 4): { size: number; path: string } {
  const qr = encode(text, { ecc: "M", border });
  let path = "";
  qr.data.forEach((row, y) => {
    let x = 0;
    while (x < row.length) {
      if (!row[x]) {
        x++;
        continue;
      }
      const start = x;
      while (x < row.length && row[x]) x++;
      path += `M${start} ${y}h${x - start}v1h${start - x}z`;
    }
  });
  return { size: qr.size, path };
}

/**
 * A QR code drawn on the spot, dark on white with a quiet zone so phone cameras
 * read it on the dark settings background. Nothing leaves the computer.
 */
export function QrCode({ text, label, className }: { text: string; label: string; className?: string }) {
  const { size, path } = useMemo(() => qrPath(text), [text]);
  return (
    <svg role="img" aria-label={label} viewBox={`0 0 ${size} ${size}`} shapeRendering="crispEdges" className={className} data-qr={text}>
      <rect width={size} height={size} fill="#fff" />
      <path d={path} fill="#0b0b0f" />
    </svg>
  );
}
