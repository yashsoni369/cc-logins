import { useEffect, useState, type ReactNode } from "react";
import { appVersion, dataLocations, openExternal } from "../lib/api";
import type { DataLocations } from "../types";

const REPO_URL = "https://github.com/yashsoni369/cc-logins";
const ISSUE_URL = `${REPO_URL}/issues/new/choose`;
const UPSTREAM_URL = "https://github.com/realiti4/claude-swap";

/** Release notes for the running build, falling back to the index when the version is unknown. */
function releaseNotesUrl(version: string | null): string {
  return version ? `${REPO_URL}/releases/tag/v${version}` : `${REPO_URL}/releases`;
}

/** `undefined` while the value is still being fetched; `null` once it is known to be unavailable. */
type Loadable<T> = T | null | undefined;

interface LinkProps {
  href: string;
  children: ReactNode;
  onFail: (message: string) => void;
}

/** Opens in the real browser — a plain `<a href>` would navigate the webview and replace the UI. */
function ExternalLink({ href, children, onFail }: LinkProps) {
  return (
    <a
      className="about-link"
      href={href}
      onClick={(e) => {
        e.preventDefault();
        // A link that does nothing when clicked must say so, not fail silently.
        openExternal(href).catch(() => onFail(`Couldn't open ${href} — open it manually.`));
      }}
    >
      {children}
    </a>
  );
}

function PathRow({ label, path }: { label: string; path: string }) {
  return (
    <div className="about-path">
      <span className="about-path-k">{label}</span>
      <span className="about-path-v num">{path}</span>
    </div>
  );
}

/**
 * What this build is and where it keeps your files. Version and paths both
 * come from the backend — neither is guessed or compiled in here.
 */
export default function AboutSection() {
  const [version, setVersion] = useState<Loadable<string>>(undefined);
  const [locations, setLocations] = useState<Loadable<DataLocations>>(undefined);
  const [linkError, setLinkError] = useState<string | null>(null);

  useEffect(() => {
    let mounted = true;
    void appVersion().then((v) => {
      if (mounted) setVersion(v);
    });
    // Degrades to "no paths" rather than throwing: this is a reader, and a
    // missing path list must not take the Settings screen down with it.
    dataLocations()
      .then((result) => {
        if (mounted) setLocations(result.data);
      })
      .catch(() => {
        if (mounted) setLocations(null);
      });
    return () => {
      mounted = false;
    };
  }, []);

  const versionLabel = version === undefined ? "…" : (version ?? "version unknown");

  return (
    <>
      <hr className="rule" />
      <div className="pane-head">
        <h3>About</h3>
      </div>

      {linkError && (
        <div className="banner danger" role="alert">
          <span>{linkError}</span>
        </div>
      )}

      <div>
        <div className="field">
          <div className="k">
            Version
            <i>The build currently running.</i>
          </div>
          <div className="v">
            <span>
              CC Logins <span className="num">{versionLabel}</span>
            </span>
          </div>
        </div>

        <div className="field">
          <div className="k">
            Links
            <i>Opens in your browser.</i>
          </div>
          <div className="v">
            <ExternalLink href={REPO_URL} onFail={setLinkError}>
              Repository
            </ExternalLink>
            <ExternalLink href={releaseNotesUrl(version ?? null)} onFail={setLinkError}>
              {version ? `Release notes for ${version}` : "Release notes"}
            </ExternalLink>
            <ExternalLink href={ISSUE_URL} onFail={setLinkError}>
              Report an issue
            </ExternalLink>
          </div>
        </div>

        <div className="field">
          <div className="k">
            Data locations
            <i>Everything this app writes lives under these paths.</i>
          </div>
          <div className="v">
            {locations === undefined && <span className="about-note">Loading…</span>}
            {locations === null && (
              <span className="about-note">
                Unavailable — only the desktop app can resolve these paths.
              </span>
            )}
            {locations && (
              <>
                <PathRow label="Account vault" path={locations.accountVault} />
                <PathRow label="Settings + history" path={locations.dataDir} />
                <PathRow label="Log file" path={locations.logFile} />
              </>
            )}
          </div>
        </div>

        <div className="field">
          <div className="k">
            Credits
            <i>MIT licensed.</i>
          </div>
          <div className="v">
            <span className="about-note">
              Portions of the credential, path, locking and usage logic are ported from{" "}
              <ExternalLink href={UPSTREAM_URL} onFail={setLinkError}>
                claude-swap
              </ExternalLink>{" "}
              by Onur Cetinkol, also MIT licensed.
            </span>
          </div>
        </div>

        <div className="field">
          <div className="k">
            Signing
            <i>Verify what you run before trusting it with tokens.</i>
          </div>
          <div className="v">
            <span className="about-note">
              Builds are currently unsigned, so Windows SmartScreen and macOS Gatekeeper will warn
              about this app.
            </span>
          </div>
        </div>
      </div>
    </>
  );
}
