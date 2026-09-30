import { Command } from "cmdk";

import Dialog from "./ui/Dialog";

export interface PaletteCommand {
  id: string;
  group: "Switch to" | "Go to" | "Actions";
  label: string;
  /** Right-aligned detail, e.g. "23% · resets 3d 21h". */
  meta?: string;
  /** Extra words that should match, e.g. the masked email of an aliased account. */
  keywords?: string[];
  run: () => void;
}

const GROUPS: PaletteCommand["group"][] = ["Switch to", "Go to", "Actions"];

/**
 * ⌘K / Ctrl+K: switch accounts, move between screens and run the common
 * actions without the mouse — the audience lives on the keyboard.
 *
 * Filtering, keyboard navigation and the combobox semantics come from `cmdk`
 * (a combobox with active-descendant is the pattern most often hand-rolled
 * wrong); the modal shell is the app's native `<dialog>`. Every command is an
 * explicit choice confirmed with Enter or a click — nothing runs on open.
 *
 * Default-exported for `React.lazy`, so `cmdk` loads the first time the
 * palette opens rather than with the app.
 */
export default function CommandPalette({ open, commands, onClose }: { open: boolean; commands: PaletteCommand[]; onClose: () => void }) {
  return (
    <Dialog open={open} onClose={onClose} label="Command palette" className="cmdk-dialog">
      <Command label="Command palette" loop>
        <div className="cmdk-q">
          <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
            <circle cx="7" cy="7" r="4.6" stroke="currentColor" strokeWidth="1.5" />
            <path d="M10.5 10.5 14 14" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
          </svg>
          <Command.Input autoFocus placeholder="Switch account, go to a screen, run an action…" />
          <span className="kbd">esc</span>
        </div>
        <Command.List className="cmdk-list">
          <Command.Empty className="cmdk-empty">No matching command.</Command.Empty>
          {GROUPS.map((group) => {
            const items = commands.filter((c) => c.group === group);
            if (items.length === 0) return null;
            return (
              <Command.Group key={group} heading={group}>
                {items.map((command) => (
                  <Command.Item
                    key={command.id}
                    value={`${group} ${command.label}`}
                    keywords={command.keywords}
                    onSelect={() => {
                      onClose();
                      command.run();
                    }}
                  >
                    <span>{command.label}</span>
                    {command.meta ? <span className="cmdk-meta num">{command.meta}</span> : null}
                  </Command.Item>
                ))}
              </Command.Group>
            );
          })}
        </Command.List>
      </Command>
    </Dialog>
  );
}
