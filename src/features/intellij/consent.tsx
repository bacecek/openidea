import { useState } from "react";
import type { IntellijServerSettings } from "@/bindings";
import { call, commands } from "@/platform/ipc";
import { openExternalUrl } from "@/platform/openUrl";
import { useProject } from "@/platform/project";
import { Select, T } from "@/ui";
import { ask } from "@/workbench/messages";
import styles from "./consent.module.css";

export const INTELLIJ_SERVER = "intellij-server";
export const INTELLIJ_FALLBACKS = ["jdtls", "kotlin-lsp", "kotlin-language-server"];
const USER_AGREEMENT = "https://www.jetbrains.com/legal/docs/toolbox/user/";
const DATA_COLLECTION = "https://www.jetbrains.com/legal/docs/terms/product_data_collection/";

const REGIONS = [
  { value: "europe", label: "Europe" },
  { value: "africa", label: "Africa" },
  { value: "americas", label: "Americas" },
  { value: "apac", label: "Asia Pacific" },
  { value: "china", label: "China" },
  { value: "middle_east", label: "Middle East" },
  { value: "oceania", label: "Oceania" },
];

const DATA_SHARING = [
  { value: "none", label: "Don't send" },
  { value: "anonymous", label: "Anonymous" },
  { value: "full", label: "Full" },
];

export type Consent = "accepted" | "declined" | "later";

export const intellijSettings = (): IntellijServerSettings =>
  useProject.getState().settings?.languages?.java?.intellijServer ?? {};

export function intellijUndecided(): boolean {
  const { acceptedTerms, declined } = intellijSettings();
  return !acceptedTerms && !declined;
}

async function save(value: IntellijServerSettings) {
  const settings = await call(commands.settingsSet("global", "languages.java.intellijServer", JSON.stringify(value)));
  useProject.setState({ settings });
}

function Link({ url, children }: { url: string; children: string }) {
  return (
    <button type="button" className={styles.link} onClick={() => void openExternalUrl(url)}>
      <T>{children}</T>
    </button>
  );
}

interface Choice {
  region: string;
  dataSharing: string;
}

function ConsentBody({ initial, onChange }: { initial: Choice; onChange: (choice: Choice) => void }) {
  const [choice, setChoice] = useState(initial);
  const set = (patch: Partial<Choice>) => {
    const next = { ...choice, ...patch };
    setChoice(next);
    onChange(next);
  };
  return (
    <div className={styles.body}>
      <p>
        <T>OpenIDEA can use IntelliJ IDEA's engine for Java and Kotlin code insight.</T>
      </p>
      <p>
        <T>
          The server is downloaded from JetBrains (about 380 MB). It includes a free 30-day trial. After that it needs
          an IntelliJ IDEA Ultimate subscription.
        </T>
      </p>
      <p>
        <T>By installing it you accept the</T> <Link url={USER_AGREEMENT}>JetBrains User Agreement</Link>.
      </p>
      <div className={styles.form}>
        <span className={styles.label}>
          <T>Region:</T>
        </span>
        <Select aria-label="Region" value={choice.region} options={REGIONS} onChange={(region) => set({ region })} />
        <span className={styles.label}>
          <T>Usage statistics:</T>
        </span>
        <Select
          aria-label="Usage statistics"
          value={choice.dataSharing}
          options={DATA_SHARING}
          onChange={(dataSharing) => set({ dataSharing })}
        />
        <span />
        <Link url={DATA_COLLECTION}>What is collected</Link>
      </div>
    </div>
  );
}

let pending: Promise<Consent> | null = null;

export function askConsent(): Promise<Consent> {
  pending ??= showConsent().finally(() => (pending = null));
  return pending;
}

async function showConsent(): Promise<Consent> {
  const current = intellijSettings();
  let choice: Choice = { region: current.region || "europe", dataSharing: current.dataSharing || "none" };
  const r = await ask({
    title: "IntelliJ IDEA for Java and Kotlin",
    body: <ConsentBody initial={choice} onChange={(c) => (choice = c)} />,
    icon: "info",
    width: 520,
    buttons: [
      { id: "accept", label: "Accept and Install", primary: true },
      { id: "decline", label: "Not now" },
    ],
  });
  if (r.button === "accept") {
    await save({ ...current, ...choice, acceptedTerms: true, declined: false });
    return "accepted";
  }
  if (r.button === "decline") {
    await save({ ...current, declined: true });
    return "declined";
  }
  return "later";
}
