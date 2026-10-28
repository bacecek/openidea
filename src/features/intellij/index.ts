import { onIdeEvent } from "@/platform/ipc";
import { useProject } from "@/platform/project";
import { useServers } from "@/features/code-insight/servers";
import { installTool, refreshTools, toolByName, useTools } from "@/features/tools/store";
import { askConsent, INTELLIJ_FALLBACKS, INTELLIJ_SERVER, intellijEnabled, intellijUndecided } from "./consent";
import { installLicensing } from "./license";

export function activate(): () => void {
  let asked = false;
  let java = false;
  let root = useProject.getState().project?.root;
  const offLicensing = installLicensing();
  const maybeAsk = () => {
    const { project, settings } = useProject.getState();
    if (asked || !java || !project || !settings || !intellijUndecided() || !intellijEnabled()) return;
    asked = true;
    void (async () => {
      if (!useTools.getState().loaded) await refreshTools();
      if ((await askConsent()) !== "accepted") return;
      if (toolByName(INTELLIJ_SERVER)?.status === "missing") await installTool(INTELLIJ_SERVER);
    })().catch((err) => console.warn("[intellij]", err));
  };
  const offConsent = onIdeEvent("languageServer", (e) => {
    if (e.status === "stopped") return;
    const languages = useServers.getState().servers.get(e.server)?.languages ?? [];
    java ||= INTELLIJ_FALLBACKS.includes(e.name) || languages.some((l) => l === "java" || l === "kotlin");
    maybeAsk();
  });
  const offSettings = useProject.subscribe((state) => {
    if (state.project?.root !== root) {
      root = state.project?.root;
      asked = false;
      java = false;
    }
    maybeAsk();
  });
  return () => {
    offConsent();
    offSettings();
    offLicensing();
  };
}
