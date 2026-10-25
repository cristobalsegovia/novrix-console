/**
 * Novrix official brand mark and lockup from `novrix-stack-design-kit/brand/`.
 * Rules:
 * - Symbol (`novrix-mark.svg`): #14161B rounded square (rx=12 on 64), white 'N',
 *   #C8F04A (`brand-lime`) underscore.
 * - Lockup: N_ followed by NOVRIX (600, 0.04em) and Console (400, graphite-muted).
 *   The lime color (#C8F04A) only exists on the underscore on a graphite background.
 */
export default function Logo({ size = 20 }: { size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 64 64"
      fill="none"
      aria-hidden="true"
    >
      <rect width="64" height="64" rx="12" fill="#14161B" />
      <path
        d="M11.182 45.0V19.08H16.978L29.614 41.616L28.282 41.94V19.08H32.818V45.0H27.022L14.386 22.464L15.718 22.14V45.0Z"
        fill="#FFFFFF"
      />
      <rect x="38.05" y="40" width="16" height="5" fill="#C8F04A" />
    </svg>
  );
}

export function NovrixLockup() {
  return (
    <span className="novrix-lockup" aria-label="Novrix Console">
      <svg
        width="34"
        height="22"
        viewBox="14 6 38 25"
        fill="none"
        aria-hidden="true"
        style={{ flexShrink: 0 }}
      >
        <path
          d="M17.612 28.0V9.28H21.798L30.924 25.556L29.962 25.79V9.28H33.238V28.0H29.052L19.926 11.724L20.888 11.49V28.0Z"
          fill="#FFFFFF"
        />
        <rect x="36.85" y="24.5" width="11" height="3.5" fill="#C8F04A" />
      </svg>
      <span className="novrix-wordmark">NOVRIX</span>
      <span className="novrix-product">Console</span>
    </span>
  );
}
