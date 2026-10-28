import { onIdeEvent } from "@/platform/ipc";
import { useProject } from "@/platform/project";
import { useServers } from "@/features/code-insight/servers";
import { installTool, refreshTools, toolByName } from "@/features/tools/store";
import { askConsent, INTELLIJ_FALLBACKS, INTELLIJ_SERVER, intellijUndecided } from "./consent";

export function activate(): () => void {
  let asked = false;
  return onIdeEvent("languageServer", (e) => {
    const languages = useServers.getState().servers.get(e.server)?.languages ?? [];
    const java = INTELLIJ_FALLBACKS.includes(e.name) || languages.some((l) => l === "java" || l === "kotlin");
    if (asked || !java || !useProject.getState().settings || !intellijUndecided()) return;
    asked = true;
    void askConsent()
      .then(async (answer) => {
        if (answer !== "accepted") return;
        await refreshTools();
        if (toolByName(INTELLIJ_SERVER)?.status === "missing") await installTool(INTELLIJ_SERVER);
      })
      .catch((err) => console.warn("[intellij]", err));
  });
}
