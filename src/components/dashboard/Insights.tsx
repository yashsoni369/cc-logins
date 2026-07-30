import type { Insight } from "@/lib/dashboard";

/**
 * The findings, in plain language.
 *
 * A dashboard that only plots numbers leaves the reader to do the reasoning,
 * and the reasoning is the part that changes a setting. Every item here is
 * derived from measurements this app took — none is advice about spending,
 * entitlements, or anything outside what was recorded.
 *
 * Renders nothing at all when nothing can be said. An empty section with a
 * reassuring heading would imply the fleet had been assessed and cleared.
 */
export default function Insights({ items }: { items: Insight[] }) {
  if (items.length === 0) return null;

  return (
    <section className="band">
      <div className="band-head">
        <h2>What this says</h2>
        <span className="sub">derived from your own history</span>
      </div>
      <div className="ins">
        {items.map((item) => (
          <div className="ins-item" key={item.id}>
            <p className="ins-hd">
              <b className={item.tone === "neutral" ? "" : item.tone}>{item.figure}</b> {item.headline}
            </p>
            <p className="ins-bd">{item.detail}</p>
          </div>
        ))}
      </div>
    </section>
  );
}
