import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { IntellijServerSettings, Settings } from "@/bindings";

const settingsSet = vi.fn(async (_scope: string, _key: string, value: string) => ({
  status: "ok" as const,
  data: { languages: { java: { intellijServer: JSON.parse(value) } } } as Settings,
}));
vi.mock("@/platform/ipc", async (orig) => {
  const actual = await orig<typeof import("@/platform/ipc")>();
  return { ...actual, commands: { ...actual.commands, settingsSet } };
});

const { emitLocalIdeEvent } = await import("@/platform/ipc");
const { useProject } = await import("@/platform/project");
const { provideNotificationService } = await import("@/workbench/contributions");
const { MessagesHost } = await import("@/workbench/messages");
const { MemoryToolsBackend, sampleTools, setToolsBackend } = await import("@/features/tools/backend");
const { cancelInstall, installTool, useTools } = await import("@/features/tools/store");
const { installMissingServerPrompts } = await import("@/features/tools/missing");
const { useServers } = await import("@/features/code-insight/servers");
const { useConsentPrompt } = await import("../consent");
const { activate } = await import("../index");

type ToolsBackend = InstanceType<typeof MemoryToolsBackend>;
let backend: ToolsBackend;
let notes: string[];
let off: () => void;

const javaTools = () => {
  const base = { ...sampleTools()[0]!, status: "missing" as const, path: null, version: null };
  return [
    ...sampleTools(),
    { ...base, name: "jdtls", displayName: "Eclipse JDT LS", command: "jdtls", languages: ["Java"] },
    {
      ...base,
      name: "intellij-server",
      displayName: "IntelliJ IDEA",
      command: "intellij-server",
      languages: ["Java", "Kotlin"],
    },
  ];
};

function decided(value: IntellijServerSettings | undefined) {
  useProject.setState({ settings: { languages: { java: value ? { intellijServer: value } : {} } } as Settings });
}

const serverEvent = (name: string, status: "missing" | "starting" = "starting") =>
  act(() => emitLocalIdeEvent({ type: "languageServer", server: 1, name, status, message: null }));

const settle = () => act(async () => void (await new Promise((r) => setTimeout(r, 20))));

beforeEach(() => {
  notes = [];
  provideNotificationService({ notify: (n) => void notes.push(n.title) });
  backend = new MemoryToolsBackend(javaTools(), 1);
  setToolsBackend(backend);
  useTools.setState({ tools: [], loaded: false, loading: false, error: null, jobs: {}, logs: {} });
  decided(undefined);
  useConsentPrompt.setState({ asking: false, dismissed: false });
  render(<MessagesHost />);
  off = activate();
});

afterEach(async () => {
  for (const b of screen.queryAllByRole("button", { name: "Cancel" })) await userEvent.click(b);
  off();
  setToolsBackend(null);
  useProject.setState({ project: null, settings: null });
  vi.clearAllMocks();
});

describe("IntelliJ IDEA consent", () => {
  it("asks once when Java or Kotlin code insight starts and the user has not decided", async () => {
    serverEvent("jdtls");
    serverEvent("kotlin-lsp", "missing");
    await settle();
    expect(screen.getAllByRole("dialog")).toHaveLength(1);
    expect(screen.getByRole("dialog")).toHaveTextContent("IntelliJ IDEA for Java and Kotlin");
  });

  it("asks once settings load when the first Java server came before them", async () => {
    useProject.setState({ settings: null });
    serverEvent("jdtls");
    await settle();
    expect(screen.queryByRole("dialog")).toBeNull();
    act(() => decided(undefined));
    await settle();
    expect(screen.getByRole("dialog")).toHaveTextContent("IntelliJ IDEA for Java and Kotlin");
  });

  it("doesn't ask when IntelliJ's server is turned off", async () => {
    useProject.setState({ settings: { languageServers: { "intellij-server": { enabled: false } } } as Settings });
    serverEvent("jdtls");
    await settle();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("asks per project when a window switches projects", async () => {
    useProject.setState({
      project: { root: "/p1" } as never,
      settings: { languageServers: { "intellij-server": { enabled: false } } } as Settings,
    });
    serverEvent("jdtls");
    await settle();
    act(() =>
      useProject.setState({ project: { root: "/p2" } as never, settings: { languages: { java: {} } } as Settings }),
    );
    await settle();
    expect(screen.queryByRole("dialog")).toBeNull();
    serverEvent("jdtls");
    await settle();
    expect(screen.getByRole("dialog")).toHaveTextContent("IntelliJ IDEA for Java and Kotlin");
  });

  it("stays quiet for other languages and once the user has decided", async () => {
    serverEvent("gopls");
    await settle();
    expect(screen.queryByRole("dialog")).toBeNull();
    off();
    for (const value of [{ acceptedTerms: true }, { declined: true }]) {
      decided(value);
      off = activate();
      serverEvent("jdtls");
      await settle();
      expect(screen.queryByRole("dialog")).toBeNull();
      off();
    }
    off = () => {};
  });

  const written = () => settingsSet.mock.calls.map(([scope, key, value]) => ({ scope, key, value: JSON.parse(value) }));

  it("Accept and Install stores the choice and downloads the server", async () => {
    serverEvent("jdtls");
    await settle();
    await userEvent.click(screen.getByRole("button", { name: "Region" }));
    await userEvent.click(screen.getByRole("option", { name: "Americas" }));
    await userEvent.click(screen.getByRole("button", { name: "Accept and Install" }));
    await waitFor(() => expect(backend.restarted).toEqual(["intellij-server"]));
    expect(written()).toEqual([
      {
        scope: "global",
        key: "languages.java.intellijServer",
        value: { acceptedTerms: true, declined: false, region: "americas", dataSharing: "none" },
      },
    ]);
  });

  it("Accept and Install skips the download when the server is already installed", async () => {
    backend.tools = backend.tools.map((t) => (t.name === "intellij-server" ? { ...t, status: "onPath" } : t));
    serverEvent("jdtls");
    await settle();
    await userEvent.click(screen.getByRole("button", { name: "Accept and Install" }));
    await settle();
    expect(written()).toHaveLength(1);
    expect(backend.restarted).toEqual([]);
  });

  it("Not now declines for good, Cancel leaves the question open", async () => {
    decided({ region: "china" });
    serverEvent("jdtls");
    await settle();
    await userEvent.click(screen.getByRole("button", { name: "Not now" }));
    await settle();
    expect(written()).toEqual([
      { scope: "global", key: "languages.java.intellijServer", value: { region: "china", declined: true } },
    ]);

    off();
    settingsSet.mockClear();
    decided(undefined);
    off = activate();
    serverEvent("jdtls");
    await settle();
    await userEvent.keyboard("{Escape}");
    await settle();
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(settingsSet).not.toHaveBeenCalled();
    expect(backend.restarted).toEqual([]);
  });

  it("holds back the jdtls install offer until the user has answered", async () => {
    const offPrompts = installMissingServerPrompts();
    serverEvent("jdtls", "missing");
    serverEvent("gopls", "missing");
    await settle();
    expect(notes).toEqual(["gopls is not installed"]);
    await userEvent.click(screen.getByRole("button", { name: "Not now" }));
    await waitFor(() => expect(notes).toEqual(["gopls is not installed", "Eclipse JDT LS is not installed"]));
    offPrompts();
  });

  it("keeps holding the jdtls install offer after the user accepts, until a later decline", async () => {
    const offPrompts = installMissingServerPrompts();
    backend.stepMs = 40;
    serverEvent("jdtls", "missing");
    await settle();
    await userEvent.click(screen.getByRole("button", { name: "Accept and Install" }));
    serverEvent("jdtls", "missing");
    serverEvent("kotlin-lsp", "missing");
    await settle();
    expect(useTools.getState().jobs["intellij-server"]).toBeDefined();
    expect(notes).toEqual([]);
    act(() => decided({ acceptedTerms: true, declined: true }));
    await waitFor(() =>
      expect(notes).toEqual(["Eclipse JDT LS is not installed", "Kotlin LSP (JetBrains) is not installed"]),
    );
    await waitFor(() => expect(backend.restarted).toEqual(["intellij-server"]));
    offPrompts();
  });

  it("offers jdtls when IntelliJ's server is installed but turned down", async () => {
    backend.tools = backend.tools.map((t) => (t.name === "intellij-server" ? { ...t, status: "installed" } : t));
    decided({ acceptedTerms: true, declined: true });
    const offPrompts = installMissingServerPrompts();
    serverEvent("jdtls", "missing");
    await waitFor(() => expect(notes).toEqual(["Eclipse JDT LS is not installed"]));
    offPrompts();
  });

  it("asks before installing the server from Language Servers", async () => {
    let result: unknown = "pending";
    void installTool("intellij-server").then((r) => (result = r));
    await settle();
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(result).toBeNull());
    expect(backend.restarted).toEqual([]);

    decided({ declined: true, region: "china" });
    const accepted = installTool("intellij-server");
    await settle();
    expect(screen.getByRole("dialog")).toHaveTextContent("IntelliJ IDEA for Java and Kotlin");
    await userEvent.click(screen.getByRole("button", { name: "Accept and Install" }));
    expect((await accepted)?.status).toBe("installed");
    expect(written().at(-1)?.value).toEqual({
      acceptedTerms: true,
      declined: false,
      region: "china",
      dataSharing: "none",
    });

    settingsSet.mockClear();
    await installTool("intellij-server");
    expect(settingsSet).not.toHaveBeenCalled();
    expect(backend.restarted).toEqual(["intellij-server", "intellij-server"]);
  });

  it("closes without writing when another window answers", async () => {
    serverEvent("jdtls");
    await settle();
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    act(() => decided({ acceptedTerms: true, region: "europe", dataSharing: "none" }));
    await settle();
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(settingsSet).not.toHaveBeenCalled();
    expect(backend.restarted).toEqual([]);
  });

  describe("jdtls install offer when IntelliJ's server isn't on its way", () => {
    const jdtlsOffer = "Eclipse JDT LS is not installed";
    let offPrompts: () => void;
    beforeEach(() => {
      offPrompts = installMissingServerPrompts();
    });
    afterEach(() => {
      offPrompts();
      useServers.setState({ servers: new Map() });
    });

    const intellijStatus = (status: "missing" | "ready" | "crashed" | "misconfigured", message: string | null = null) =>
      act(() =>
        useServers.setState({
          servers: new Map([
            [7, { id: 7, name: "intellij-server", status, message, languages: ["java", "kotlin"], root: null }],
          ]),
        }),
      );
    const installed = () =>
      (backend.tools = backend.tools.map((t) => (t.name === "intellij-server" ? { ...t, status: "installed" } : t)));

    it("shows the held offer when the download fails", async () => {
      backend.failNext = "intellij-server";
      serverEvent("jdtls", "missing");
      await settle();
      await userEvent.click(screen.getByRole("button", { name: "Accept and Install" }));
      await waitFor(() => expect(notes).toContain(jdtlsOffer));
      expect(notes).toContain("Failed to install IntelliJ IDEA");
    });

    it("shows the held offer when the download is cancelled", async () => {
      backend.stepMs = 30;
      serverEvent("jdtls", "missing");
      await settle();
      await userEvent.click(screen.getByRole("button", { name: "Accept and Install" }));
      await waitFor(() => expect(useTools.getState().jobs["intellij-server"]).toBeDefined());
      expect(notes).toEqual([]);
      act(() => cancelInstall("intellij-server"));
      await waitFor(() => expect(notes).toEqual([jdtlsOffer]));
    });

    it("shows the held offer when the consent dialog is dismissed", async () => {
      serverEvent("jdtls", "missing");
      await settle();
      expect(notes).toEqual([]);
      await userEvent.keyboard("{Escape}");
      await waitFor(() => expect(notes).toEqual([jdtlsOffer]));
    });

    it("offers jdtls once IntelliJ's server crashes or gives up", async () => {
      installed();
      decided({ acceptedTerms: true });
      intellijStatus("ready");
      serverEvent("jdtls", "missing");
      await settle();
      expect(notes).toEqual([]);
      intellijStatus("crashed", "intellij-server exited with code 1\nRestarting…");
      await settle();
      expect(notes).toEqual([]);
      intellijStatus("crashed", "intellij-server exited with code 1\nCrashed repeatedly; restart it manually.");
      await waitFor(() => expect(notes).toEqual([jdtlsOffer]));
    });

    it("offers jdtls when IntelliJ's server can't start", async () => {
      installed();
      decided({ acceptedTerms: true });
      intellijStatus("ready");
      serverEvent("jdtls", "missing");
      await settle();
      intellijStatus("misconfigured");
      await waitFor(() => expect(notes).toEqual([jdtlsOffer]));
    });

    it("keeps the offer back while the downloaded server starts", async () => {
      intellijStatus("missing");
      serverEvent("jdtls", "missing");
      await settle();
      await userEvent.click(screen.getByRole("button", { name: "Accept and Install" }));
      await waitFor(() => expect(notes).toContain("IntelliJ IDEA installed"));
      await settle();
      intellijStatus("ready");
      await settle();
      expect(notes).toEqual(["IntelliJ IDEA installed"]);
    });

    it("offers jdtls when IntelliJ's server is turned off", async () => {
      installed();
      useProject.setState({
        settings: {
          languages: { java: { intellijServer: { acceptedTerms: true } } },
          languageServers: { "intellij-server": { enabled: false } },
        } as Settings,
      });
      serverEvent("jdtls", "missing");
      await waitFor(() => expect(notes).toEqual([jdtlsOffer]));
    });
  });
});
