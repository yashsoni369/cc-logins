export interface SegOption<T extends string> {
  id: T;
  label: string;
}

/**
 * A `.seg` segmented control: a radiogroup of spans, operable by click and
 * Enter/Space. Moved out of SettingsScreen so the popover and Settings share
 * one implementation.
 */
export default function Segmented<T extends string>({
  options,
  value,
  onChange,
  ariaLabel,
  disabled = false,
  compact = false,
}: {
  options: Array<SegOption<T>>;
  value: T;
  onChange: (v: T) => void;
  ariaLabel: string;
  disabled?: boolean;
  compact?: boolean;
}) {
  return (
    <div className={`seg${compact ? " seg-compact" : ""}`} role="radiogroup" aria-label={ariaLabel} aria-disabled={disabled || undefined}>
      {options.map((opt) => (
        <span
          key={opt.id}
          role="radio"
          aria-checked={opt.id === value}
          tabIndex={disabled ? -1 : 0}
          className={opt.id === value ? "on" : undefined}
          onClick={() => {
            if (!disabled) onChange(opt.id);
          }}
          onKeyDown={(e) => {
            if (disabled) return;
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              onChange(opt.id);
            }
          }}
        >
          {opt.label}
        </span>
      ))}
    </div>
  );
}
