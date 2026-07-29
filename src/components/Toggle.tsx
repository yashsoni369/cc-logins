import type { KeyboardEvent, MouseEvent } from "react";

/**
 * The app's one switch: a `.toggle`/`.sw` control, made operable by click or
 * Enter/Space. Promoted out of `SettingsScreen` so the accounts table and the
 * settings pane cannot drift into two switches that behave differently.
 *
 * `role="switch"` on a span buys none of `<button>`'s free behaviour, so
 * disabling is done by hand — no `onChange`, and out of the tab order — rather
 * than by an attribute the browser would honour.
 */
interface ToggleProps {
  checked: boolean;
  onChange: (next: boolean) => void;
  /** Visible text beside the switch; omit for a bare switch. */
  label?: string;
  /** Accessible name when there is no visible label — required in that case. */
  ariaLabel?: string;
  title?: string;
  disabled?: boolean;
  /** Dimmed and `aria-busy` mid-action, at unchanged size so nothing reflows. */
  pending?: boolean;
  /** Keep click and Enter/Space off a clickable ancestor, e.g. an expandable row. */
  stopPropagation?: boolean;
}

export default function Toggle({
  checked,
  onChange,
  label,
  ariaLabel,
  title,
  disabled = false,
  pending = false,
  stopPropagation = false,
}: ToggleProps) {
  // A pending flip is still in flight; a second one would race the first.
  const inert = disabled || pending;

  const className = [
    "toggle",
    label === undefined ? "toggle-bare" : null,
    pending ? "is-pending" : null,
  ]
    .filter(Boolean)
    .join(" ");

  function onClick(e: MouseEvent<HTMLSpanElement>) {
    // Stop even when inert: the ancestor must not act on a click the switch ate.
    if (stopPropagation) e.stopPropagation();
    if (inert) return;
    onChange(!checked);
  }

  function onKeyDown(e: KeyboardEvent<HTMLSpanElement>) {
    if (e.key !== "Enter" && e.key !== " ") return;
    e.preventDefault();
    if (stopPropagation) e.stopPropagation();
    if (inert) return;
    onChange(!checked);
  }

  return (
    <span
      className={className}
      role="switch"
      aria-checked={checked}
      aria-disabled={disabled || undefined}
      aria-busy={pending || undefined}
      aria-label={label === undefined ? ariaLabel : undefined}
      title={title}
      tabIndex={disabled ? -1 : 0}
      onClick={onClick}
      onKeyDown={onKeyDown}
    >
      <span className={`sw${checked ? " on" : ""}`}></span>
      {label}
    </span>
  );
}
