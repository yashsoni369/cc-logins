import { formatSpend, isEnterprise, type Usage } from "@/types";

/**
 * The `[E]` mark on an enterprise account.
 *
 * These accounts are limited by a monthly spend cap and have no rate-limit
 * windows at all, so several columns elsewhere mean something different on
 * their rows. This is what says so at a glance.
 *
 * It has its own colour token rather than borrowing `--caution`. Yellow is
 * spoken for in this app: it means 75–89% utilisation. A badge in that exact
 * hue would make an enterprise account at 2% read as one nearing its limit,
 * which is the opposite of what the badge is for. `--plan-badge` sits at a
 * warmer hue so the two never trade places.
 */
export default function PlanBadge({ usage }: { usage: Usage | undefined }) {
  if (!isEnterprise(usage)) return null;

  const spend = usage?.spend;
  const title = spend
    ? `Enterprise — limited by a monthly spend cap, ${formatSpend(spend)} used. ` +
      "This plan has no 5-hour or 7-day windows."
    : "Enterprise — limited by a monthly spend cap rather than 5-hour or 7-day windows.";

  return (
    <span className="plan-badge" title={title} aria-label={title}>
      E
    </span>
  );
}
