import { RANGES, RANGE_ORDER, type RangeKey } from "@/lib/dashboard";

interface RangePickerProps {
  value: RangeKey;
  onChange: (next: RangeKey) => void;
}

/**
 * The one control every band reads from.
 *
 * Deliberately quiet text toggles rather than buttons or a select: this sits
 * beside a page heading, and a control that shouts competes with the figures
 * it is there to scope.
 */
export default function RangePicker({ value, onChange }: RangePickerProps) {
  return (
    <div className="rangepick" role="group" aria-label="Time range">
      {RANGE_ORDER.map((key, i) => (
        <span key={key}>
          {i > 0 && <span className="dot" aria-hidden="true">·</span>}
          <button
            type="button"
            aria-pressed={key === value}
            onClick={() => onChange(key)}
          >
            {RANGES[key].label}
          </button>
        </span>
      ))}
    </div>
  );
}
