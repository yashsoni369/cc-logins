import { useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";

export interface MenuItem {
  id: string;
  label: ReactNode;
  /** Secondary line under the label. */
  description?: ReactNode;
  disabled?: boolean;
  title?: string;
  tone?: "danger";
  onSelect: () => void;
}

interface MenuButtonProps {
  /** Button contents. */
  children: ReactNode;
  items: MenuItem[];
  buttonClassName?: string;
  ariaLabel?: string;
  disabled?: boolean;
  align?: "start" | "end";
  menuClassName?: string;
}

/**
 * WAI-ARIA menu button: `aria-haspopup`/`aria-expanded` on the trigger;
 * Enter, Space or ArrowDown opens with focus on the first item; arrows move,
 * Home/End jump, Escape closes and returns focus, Tab closes.
 *
 * Clicks and keys inside never bubble to a clickable ancestor (e.g. a row
 * that opens a drawer).
 */
export default function MenuButton({
  children,
  items,
  buttonClassName = "btn",
  ariaLabel,
  disabled,
  align = "end",
  menuClassName,
}: MenuButtonProps) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const itemRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const menuId = useId();

  const enabledIndexes = items.map((item, i) => (item.disabled ? -1 : i)).filter((i) => i >= 0);

  const focusItem = (index: number | undefined) => {
    if (index !== undefined) itemRefs.current[index]?.focus();
  };

  useEffect(() => {
    if (!open) return;
    focusItem(enabledIndexes[0]);
    const onDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
    // Focus the first item once, when the menu opens.
  }, [open]);

  const close = (refocus: boolean) => {
    setOpen(false);
    if (refocus) buttonRef.current?.focus();
  };

  const onMenuKey = (e: KeyboardEvent<HTMLDivElement>) => {
    e.stopPropagation();
    const current = itemRefs.current.findIndex((el) => el === document.activeElement);
    const pos = enabledIndexes.indexOf(current);
    if (e.key === "ArrowDown") {
      e.preventDefault();
      focusItem(enabledIndexes[(pos + 1) % enabledIndexes.length]);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      focusItem(enabledIndexes[(pos - 1 + enabledIndexes.length) % enabledIndexes.length]);
    } else if (e.key === "Home") {
      e.preventDefault();
      focusItem(enabledIndexes[0]);
    } else if (e.key === "End") {
      e.preventDefault();
      focusItem(enabledIndexes[enabledIndexes.length - 1]);
    } else if (e.key === "Escape") {
      e.preventDefault();
      close(true);
    } else if (e.key === "Tab") {
      close(false);
    }
  };

  return (
    <div
      className="menubtn"
      ref={rootRef}
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => e.stopPropagation()}
    >
      <button
        ref={buttonRef}
        type="button"
        className={buttonClassName}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        aria-label={ariaLabel}
        disabled={disabled}
        onClick={() => setOpen((v) => !v)}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown") {
            e.preventDefault();
            setOpen(true);
          }
        }}
      >
        {children}
      </button>
      {open && (
        <div id={menuId} role="menu" className={`menu menu-${align}${menuClassName ? ` ${menuClassName}` : ""}`} onKeyDown={onMenuKey}>
          {items.map((item, i) => (
            <button
              key={item.id}
              ref={(el) => {
                itemRefs.current[i] = el;
              }}
              type="button"
              role="menuitem"
              tabIndex={-1}
              className={`menu-item${item.tone === "danger" ? " danger" : ""}`}
              disabled={item.disabled}
              title={item.title}
              onClick={() => {
                close(true);
                item.onSelect();
              }}
            >
              <span className="menu-label">{item.label}</span>
              {item.description && <span className="menu-desc">{item.description}</span>}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
