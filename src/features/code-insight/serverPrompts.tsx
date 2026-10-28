import type { LspConflict, LspPrompt, Range, ServerId } from "@/bindings";
import { activeEditor } from "@/editor/api";
import { runEditorCommand } from "@/editor/lsp/completion";
import { call, commands, onIdeEvent } from "@/platform/ipc";
import { relativePath, useProject } from "@/platform/project";
import { projectRelative } from "@/platform/projectRoots";
import { copyText } from "@/ui";
import { editorService } from "@/workbench/contributions";
import { chooseItem } from "@/workbench/inputDialogs";
import { ask } from "@/workbench/messages";
import { useServers } from "./servers";
import styles from "./serverPrompts.module.css";

const selection = (r: Range | null) =>
  r ? { line: r.start.line, col: r.start.col, endLine: r.end.line, endCol: r.end.col } : undefined;

const plainText = (html: string) => new DOMParser().parseFromString(html, "text/html").body.textContent ?? "";

function Conflicts({ conflicts }: { conflicts: LspConflict[] }) {
  const root = useProject.getState().project?.root ?? "";
  return (
    <div className={styles.conflicts}>
      {conflicts.map((c, i) => (
        <p key={i}>
          {c.messages.map(plainText).join(" ")}
          {c.path && (
            <>
              {" "}
              <button
                type="button"
                className={styles.link}
                onClick={() => void editorService().openFile(c.path!, { selection: selection(c.range) })}
              >
                {relativePath(root, c.path)}
                {c.range && `:${c.range.start.line + 1}`}
              </button>
            </>
          )}
        </p>
      ))}
    </div>
  );
}

async function answer(server: ServerId, prompt: LspPrompt, signal: AbortSignal): Promise<string | null> {
  if (prompt.kind === "conflicts") {
    const r = await ask({
      title: prompt.title,
      body: <Conflicts conflicts={prompt.conflicts} />,
      signal,
      icon: "warning",
      buttons: [
        { id: "continue", label: prompt.continueLabel || "Continue", primary: true },
        { id: "cancel", label: prompt.cancelLabel || "Cancel" },
      ],
    });
    return r.button === "continue" ? "continue" : null;
  }
  if (prompt.kind === "chooseAction")
    return chooseItem({ title: prompt.title || "Choose an Action", items: prompt.items, signal });
  const r = await ask({
    title: useServers.getState().servers.get(server)?.name ?? "",
    body: prompt.message,
    icon: prompt.level,
    signal,
    buttons: prompt.actions.map((label, i) => ({ id: String(i), label, primary: i === 0 })),
  });
  return prompt.actions[Number(r.button)] ?? null;
}

export function installServerPrompts(): () => void {
  const open = new Map<number, AbortController>();
  const disposers = [
    onIdeEvent("lspPrompt", (e) => {
      if (!useServers.getState().servers.has(e.server)) return;
      const closed = new AbortController();
      open.set(e.id, closed);
      void answer(e.server, e.prompt, closed.signal).then((reply) => {
        open.delete(e.id);
        if (!closed.signal.aborted) void call(commands.lspPromptReply(e.id, reply)).catch(() => {});
      });
    }),
    onIdeEvent("lspPromptClosed", (e) => open.get(e.id)?.abort()),
    onIdeEvent("copyToClipboard", (e) => void copyText(e.text)),
    onIdeEvent("runEditorCommand", (e) => {
      const ed = activeEditor();
      if (ed && ed.ctx.path === e.path) runEditorCommand(ed.view, e.command);
    }),
    onIdeEvent("showDocument", (e) => {
      if (projectRelative(e.path) === null) return;
      void editorService().openFile(e.path, { selection: selection(e.selection), focus: e.takeFocus });
    }),
  ];
  return () => disposers.forEach((d) => d());
}
