import { onIdeEvent } from "@/platform/ipc";
import { useProject } from "@/platform/project";
import { useServers } from "@/features/code-insight/servers";
import { installTool, refreshTools, toolByName, useTools } from "@/features/tools/store";
import { askConsent, INTELLIJ_FALLBACKS, INTELLIJ_SERVER, intellijEnabled, intellijUndecided } from "./consent";
import { installLicensing } from "./license";

export function activate(): () => void {
  let asked = false;
  let java = false;
  const offLicensing = installLicensing();
  const maybeAsk = () => {
    if (asked || !java || !useProject.getState().settings || !intellijUndecided() || !intellijEnabled()) return;
    asked = true;
    void (async () => {
      if (!useTools.getState().loaded) await refreshTools();
      if ((await askConsent()) !== "accepted") return;
      if (toolByName(INTELLIJ_SERVER)?.status === "missing") await installTool(INTELLIJ_SERVER);
    })().catch((err) => console.warn("[intellij]", err));
  };
  const offConsent = onIdeEvent("languageServer", (e) => {
    const languages = useServers.getState().servers.get(e.server)?.languages ?? [];
    java ||= INTELLIJ_FALLBACKS.includes(e.name) || languages.some((l) => l === "java" || l === "kotlin");
    maybeAsk();
  });
  const offSettings = useProject.subscribe(maybeAsk);
  return () => {
    offConsent();
    offSettings();
    offLicensing();
  };
}
