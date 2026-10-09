import Segmented from "../ui/Segmented";
import type { DaemonPhase } from "../../types";

type Mode = "off" | "on" | "hold";

const OPTIONS: Array<{ id: Mode; label: string }> = [
  { id: "off", label: "Off" },
  { id: "on", label: "On" },
  { id: "hold", label: "Hold 1h" },
];

/**
 * Auto-switch as a control instead of a status line: Off, On, or held for an
 * hour. The selected value is read from the backend's daemon phase, never
 * assumed from the click, so the control can't claim a state the daemon is
 * not in.
 */
export default function AutoSwitchControl({
  phase,
  onEnable,
  onDisable,
  onHold,
  disabled,
}: {
  phase: DaemonPhase | undefined;
  onEnable: () => void;
  onDisable: () => void;
  onHold: () => void;
  disabled: boolean;
}) {
  const value: Mode = phase?.kind === "disabled" ? "off" : phase?.kind === "paused" ? "hold" : "on";
  return (
    <Segmented
      compact
      ariaLabel="Auto-switch"
      value={value}
      disabled={disabled}
      options={OPTIONS}
      onChange={(next) => {
        if (next === value) return;
        if (next === "off") onDisable();
        else if (next === "hold") onHold();
        else onEnable();
      }}
    />
  );
}
