import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { LspPrompt } from "@/bindings";

const lspPromptReply = vi.fn(async (_id: number, _answer: string | null) => ({ status: "ok" as const, data: null }));
vi.mock("@/platform/ipc", async (orig) => {
  const actual = await orig<typeof import("@/platform/ipc")>();
  return { ...actual, commands: { ...actual.commands, lspPromptReply } };
});

const startRename = vi.fn(async (_view: unknown) => {});
const triggerSignatureHelp = vi.fn((_view: unknown) => {});
const startCompletion = vi.fn((_view: unknown) => true);
vi.mock("@/editor/lsp/rename", () => ({ startRename }));
vi.mock("@/editor/lsp/signature", async (orig) => ({
  ...(await orig<typeof import("@/editor/lsp/signature")>()),
  triggerSignatureHelp,
}));
vi.mock("@codemirror/autocomplete", async (orig) => ({
  ...(await orig<typeof import("@codemirror/autocomplete")>()),
  startCompletion,
}));

const { emitLocalIdeEvent } = await import("@/platform/ipc");
const { _setActiveEditor } = await import("@/editor/api");
const { EditorView } = await import("@codemirror/view");
const { useProject } = await import("@/platform/project");
const { provideEditorService } = await import("@/workbench/contributions");
const { useServers } = await import("./servers");
const { MessagesHost } = await import("@/workbench/messages");
const { InputDialogsHost } = await import("@/workbench/inputDialogs");
const { installServerPrompts } = await import("./serverPrompts");

let off: () => void;
const openFile = vi.fn(async (_path: string, _options?: unknown) => {});

beforeEach(() => {
  provideEditorService({ openFile } as never);
  useProject.setState({ project: { root: "/p", name: "p" } as never });
  useServers.setState({
    servers: new Map([
      [1, { id: 1, name: "intellij-server", status: "ready", message: null, languages: ["java"], root: null }],
    ]),
  });
  render(
    <>
      <MessagesHost />
      <InputDialogsHost />
    </>,
  );
  off = installServerPrompts();
});

afterEach(async () => {
  for (let i = 0; i < 5 && screen.queryAllByRole("dialog").length; i++)
    for (const b of screen.queryAllByRole("button", { name: /^Cancel$/ })) await userEvent.click(b);
  off();
  useServers.setState({ servers: new Map() });
  useProject.setState({ project: null });
  _setActiveEditor(null);
  vi.clearAllMocks();
});

const ask = (prompt: LspPrompt, id = 1, server = 1) =>
  act(() => emitLocalIdeEvent({ type: "lspPrompt", id, server, prompt }));

describe("language server prompts", () => {
  it("asks a message request and replies with the chosen action", async () => {
    ask({ kind: "messageRequest", level: "info", message: "Which build to import?", actions: ["Gradle", "Maven"] });
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Which build to import?")).toBeTruthy();
    await userEvent.click(within(dialog).getByRole("button", { name: "Maven" }));
    await vi.waitFor(() => expect(lspPromptReply).toHaveBeenCalledWith(1, "Maven"));
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("replies null when the message request is dismissed", async () => {
    ask({ kind: "messageRequest", level: "warning", message: "Import?", actions: ["Gradle"] });
    await screen.findByRole("dialog");
    await userEvent.keyboard("{Escape}");
    await vi.waitFor(() => expect(lspPromptReply).toHaveBeenCalledWith(1, null));
  });

  it("leaves prompts of another window's servers alone", async () => {
    ask({ kind: "messageRequest", level: "info", message: "Import?", actions: ["Gradle"] }, 2, 7);
    await act(async () => await new Promise((r) => setTimeout(r, 20)));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(lspPromptReply).not.toHaveBeenCalled();
  });

  it("lists refactoring conflicts as plain text with locations and replies continue", async () => {
    ask({
      kind: "conflicts",
      title: "Problems Detected",
      conflicts: [
        {
          messages: ["Method <b>run</b> is already defined in class <code>Main</code>", "x &lt; y &amp;&amp; z"],
          path: "/p/src/Main.java",
          range: { start: { line: 4, col: 2 }, end: { line: 4, col: 5 } },
        },
        { messages: ["Conflict without a place"], path: null, range: null },
      ],
      continueLabel: "Refactor Anyway",
      cancelLabel: "Stop",
    });
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Problems Detected")).toBeTruthy();
    expect(dialog.textContent).toContain("Method run is already defined in class Main");
    expect(dialog.textContent).toContain("x < y && z");
    expect(dialog.textContent).toContain("Conflict without a place");
    expect(dialog.innerHTML).not.toContain("<b>");
    await userEvent.click(within(dialog).getByRole("button", { name: "src/Main.java:5" }));
    expect(openFile).toHaveBeenCalledWith("/p/src/Main.java", {
      selection: { line: 4, col: 2, endLine: 4, endCol: 5 },
    });
    expect(screen.getByRole("dialog")).toBe(dialog);
    expect(within(dialog).getByRole("button", { name: "Stop" })).toBeTruthy();
    await userEvent.click(within(dialog).getByRole("button", { name: "Refactor Anyway" }));
    await vi.waitFor(() => expect(lspPromptReply).toHaveBeenCalledWith(1, "continue"));
  });

  it("falls back to Continue and Cancel and replies null when conflicts are cancelled", async () => {
    ask({
      kind: "conflicts",
      title: "Conflicts",
      conflicts: [{ messages: ["a"], path: null, range: null }],
      continueLabel: "",
      cancelLabel: "",
    });
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByRole("button", { name: "Continue" })).toBeTruthy();
    await userEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await vi.waitFor(() => expect(lspPromptReply).toHaveBeenCalledWith(1, null));
  });

  it("lets the user choose a quick fix variant and replies with it", async () => {
    ask({ kind: "chooseAction", title: "", items: ["Import class", "Create class"] });
    const dialog = await screen.findByRole("dialog", { name: "Choose an Action" });
    await userEvent.dblClick(within(dialog).getByText("Create class"));
    await vi.waitFor(() => expect(lspPromptReply).toHaveBeenCalledWith(1, "Create class"));
  });

  it("replies null when the choice is dismissed", async () => {
    ask({ kind: "chooseAction", title: "Add import", items: ["a.Foo", "b.Foo"] });
    await screen.findByRole("dialog", { name: "Add import" });
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await vi.waitFor(() => expect(lspPromptReply).toHaveBeenCalledWith(1, null));
  });

  it("closes the open dialog when the prompt is answered elsewhere", async () => {
    ask({ kind: "messageRequest", level: "info", message: "Import?", actions: ["Gradle"] }, 3);
    ask({ kind: "chooseAction", title: "Add import", items: ["a.Foo"] }, 4);
    await screen.findByRole("dialog", { name: "Add import" });
    act(() => emitLocalIdeEvent({ type: "lspPromptClosed", id: 5 }));
    expect(screen.getAllByRole("dialog")).toHaveLength(2);
    act(() => emitLocalIdeEvent({ type: "lspPromptClosed", id: 3 }));
    act(() => emitLocalIdeEvent({ type: "lspPromptClosed", id: 4 }));
    await vi.waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await act(async () => await new Promise((r) => setTimeout(r, 20)));
    expect(lspPromptReply).not.toHaveBeenCalled();
  });
});

describe("server editor requests", () => {
  const settle = () => act(async () => await new Promise((r) => setTimeout(r, 20)));

  it("copies text to the clipboard", async () => {
    const writeText = vi.fn(async (_text: string) => {});
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
    act(() => emitLocalIdeEvent({ type: "copyToClipboard", text: "com.example.Main" }));
    await vi.waitFor(() => expect(writeText).toHaveBeenCalledWith("com.example.Main"));
  });

  it("runs editor commands only in the editor showing the server's file", async () => {
    const view = new EditorView({ doc: "class Main {}", parent: document.body });
    _setActiveEditor({ view, ctx: { doc: 1, path: "/p/Main.java", language: "java", uiMode: null, readOnly: false } });
    const run = async (command: string, path: string | null) => {
      act(() => emitLocalIdeEvent({ type: "runEditorCommand", command, path }));
      await settle();
    };
    await run("editor.action.rename", "/p/Other.java");
    await run("editor.action.rename", null);
    expect(startRename).not.toHaveBeenCalled();
    await run("editor.action.rename", "/p/Main.java");
    expect(startRename).toHaveBeenCalledWith(view);
    await run("editor.action.triggerSuggest", "/p/Main.java");
    expect(startCompletion).toHaveBeenCalledWith(view);
    await run("editor.action.triggerParameterHints", "/p/Main.java");
    expect(triggerSignatureHelp).toHaveBeenCalledWith(view);
    view.destroy();
  });

  it("opens and selects a document the server shows, only for this project", async () => {
    const range = { start: { line: 2, col: 4 }, end: { line: 2, col: 9 } };
    act(() => emitLocalIdeEvent({ type: "showDocument", path: "/q/Main.java", selection: range, takeFocus: true }));
    expect(openFile).not.toHaveBeenCalled();
    act(() => emitLocalIdeEvent({ type: "showDocument", path: "/p/src/Main.java", selection: range, takeFocus: true }));
    expect(openFile).toHaveBeenCalledWith("/p/src/Main.java", {
      selection: { line: 2, col: 4, endLine: 2, endCol: 9 },
      focus: true,
    });
  });
});
