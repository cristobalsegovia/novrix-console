/**
 * Switchboard mark — a 3×2 grid of black jack dots on a sky-blue tile, with a
 * diagonal patch cord descending from the top-left dot through the centre and
 * thickening into a jack node toward the lower-right. Traced from the source
 * logo's own geometry. Brand colors are fixed (not theme-dependent).
 */
const TILE = "#4fb3e8";
const DOT = "#101418";

export default function Logo({ size = 16 }: { size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 48 48"
      fill="none"
      aria-hidden="true"
    >
      <rect x="1" y="1" width="46" height="46" rx="12" fill={TILE} />
      {/* Diagonal cord: top-left dot, down through centre, to lower-right node */}
      <path
        d="M 16.5 16.5 L 24 24 L 30 30"
        stroke={DOT}
        strokeWidth="2.8"
        strokeLinecap="round"
        strokeLinejoin="round"
        fill="none"
      />
      {/* Top row: three dots */}
      <circle cx="16.5" cy="16.5" r="2.6" fill={DOT} />
      <circle cx="24" cy="16" r="2.6" fill={DOT} />
      <circle cx="31.5" cy="16" r="2.6" fill={DOT} />
      {/* Bottom row: left dot, small centre, big node lower-right */}
      <circle cx="16" cy="31.5" r="2.6" fill={DOT} />
      <circle cx="23.5" cy="31.5" r="2.2" fill={DOT} />
      <circle cx="30.5" cy="30" r="3.6" fill={DOT} />
    </svg>
  );
}
