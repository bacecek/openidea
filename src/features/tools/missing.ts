/**
 * "rust-analyzer is not installed — Install": when a language server reports
 * `missing` and we know how to install it, offer a one-click install that
 * restarts the server when done. Uses the same notification key as the
 * generic code-insight notice so it replaces it.
 */
import { executeAction } from "@/platform/actions";
import { onIdeEvent } from "@/platform/ipc";
import { useProject } from "@/platform/project";
import type { ServerStatus } from "@/bindings";
import { useServers } from "@/features/code-insight/servers";
import { INTELLIJ_FALLBACKS, INTELLIJ_SERVER, intellijSettings, useConsentPrompt } from "@/features/intellij/consent";
import { notifications } from "@/workbench/contributions";
import { installTool, refreshTools, SETTINGS_PAGE_ID, toolByName, useTools } from "./store";
import { tx } from "@/platform/i18n";

export const missingKey = (name: string) => `lsp-missing-${name}`;

const NOT_SERVING: ServerStatus[] = ["missing", "crashed", "misconfigured"];

const intellijEnabled = () => useProject.getState().settings?.languageServers?.[INTELLIJ_SERVER]?.enabled !== false;

function intellijServes(): boolean {
  const { acceptedTerms, declined } = intellijSettings();
  if (!acceptedTerms || declined || !intellijEnabled() || toolByName(INTELLIJ_SERVER)?.status === "missing")
    return false;
  return ![...useServers.getState().servers.values()].some(
    (s) => s.name === INTELLIJ_SERVER && NOT_SERVING.includes(s.status),
  );
}

function intellijOnItsWay(): boolean {
  if (!useProject.getState().settings) return true;
  const { acceptedTerms, declined } = intellijSettings();
  if (declined || !intellijEnabled()) return false;
  const consent = useConsentPrompt.getState();
  if (consent.asking || useTools.getState().jobs[INTELLIJ_SERVER]) return true;
  return acceptedTerms ? intellijServes() : !consent.dismissed;
}

export async function promptInstall(name: string): Promise<boolean> {
  if (!useTools.getState().loaded) await refreshTools();
  const tool = toolByName(name);
  if (!tool || !tool.canInstall || tool.status !== "missing") return false;
  // An installed alternative (typescript-language-server for vtsls) already serves these files.
  const served = useTools
    .getState()
    .tools.some(
      (t) =>
        t.name !== name &&
        (t.name !== INTELLIJ_SERVER || intellijServes()) &&
        t.kind === tool.kind &&
        t.status !== "missing" &&
        t.languages.some((l) => tool.languages.includes(l)),
    );
  if (served) return false;
  const key = missingKey(name);
  const langs = tool.languages.length ? tool.languages.join(", ") : "these files";
  notifications().notify({
    group: "tools",
    level: "warning",
    title: `${tool.displayName} is not installed`,
    body: tx`Code insight for ${langs} needs ${tool.command}. Install it via ${tool.installMethod}?`,
    key,
    sticky: true,
    actions: [
      { label: "Install", run: () => void installTool(name, { notificationKey: key }) },
      { label: "Language Servers…", run: () => void executeAction("file.settings", SETTINGS_PAGE_ID) },
    ],
  });
  return true;
}

export function installMissingServerPrompts(): () => void {
  const prompted = new Set<string>();
  const waiting = new Set<string>();
  const prompt = (name: string) => {
    prompted.add(name);
    // Async on purpose: lands after the synchronous generic notice and replaces it.
    void promptInstall(name).catch(() => {});
  };
  const release = () => {
    if (!waiting.size || intellijOnItsWay()) return;
    const names = [...waiting];
    waiting.clear();
    names.forEach(prompt);
  };
  let timer: ReturnType<typeof setTimeout> | undefined;
  const schedule = () => {
    clearTimeout(timer);
    timer = setTimeout(release, 0);
  };
  const offEvents = onIdeEvent("languageServer", (e) => {
    if (e.status === "missing" && !prompted.has(e.name)) {
      if (!INTELLIJ_FALLBACKS.includes(e.name)) prompt(e.name);
      else {
        waiting.add(e.name);
        if (!useTools.getState().loaded) void refreshTools();
      }
    }
    schedule();
  });
  const unsubs = [useProject, useTools, useServers, useConsentPrompt].map((store) => store.subscribe(schedule));
  return () => {
    offEvents();
    unsubs.forEach((u) => u());
    clearTimeout(timer);
  };
}
