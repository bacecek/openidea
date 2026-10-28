import { describe, expect, it, vi } from "vitest";
import { isVirtualUri, virtualContainer, virtualDocument, virtualName, virtualParent } from "./virtualDocs";
import { basename, dirname, relativeDir, relativePath } from "./project";

const lspVirtualDocument = vi.hoisted(() =>
  vi.fn(async (uri: string) => ({
    status: "ok" as const,
    data: {
      uri,
      text: "",
      languageId: "java",
      title: "Strings.class",
      description: "/Users/me/My Libs/guava-33.jar",
      decompiled: true,
    },
  })),
);
vi.mock("./ipc", async (orig) => {
  const actual = await orig<typeof import("./ipc")>();
  return { ...actual, commands: { ...actual.commands, lspVirtualDocument } };
});

const JDT =
  "jdt://contents/spring-boot-4.1.1.jar/org.springframework.boot/SpringApplication.class?=demo/%5C/m2%5C/spring-boot-4.1.1.jar%3Corg.springframework.boot%28SpringApplication.class";

describe("virtual document URIs", () => {
  it("tells URIs from file paths", () => {
    expect(isVirtualUri(JDT)).toBe(true);
    expect(isVirtualUri("untitled:Untitled-1")).toBe(true);
    expect(isVirtualUri("/Users/me/a.java")).toBe(false);
    expect(isVirtualUri("C:\\src\\a.java")).toBe(false);
    expect(isVirtualUri("file:///a.java")).toBe(false);
    expect(isVirtualUri(null)).toBe(false);
  });

  it("names and locates library classes", () => {
    expect(virtualName(JDT)).toBe("SpringApplication.class");
    expect(virtualParent(JDT)).toBe("jdt://contents/spring-boot-4.1.1.jar/org.springframework.boot");
    expect(virtualContainer(JDT)).toBe("spring-boot-4.1.1.jar › org.springframework.boot");
  });

  it("makes path helpers URI-aware", () => {
    expect(basename(JDT)).toBe("SpringApplication.class");
    expect(basename(dirname(JDT))).toBe("org.springframework.boot");
    expect(relativePath("/p", JDT)).toBe("spring-boot-4.1.1.jar › org.springframework.boot › SpringApplication.class");
    expect(relativeDir("/p", JDT)).toBe("spring-boot-4.1.1.jar › org.springframework.boot");
    expect(relativePath("/p", "/p/src/A.java")).toBe("src/A.java");
  });

  it("labels intellij-server library sources with the server's description", async () => {
    const jar = "jar:file:///Users/me/My%20Libs/guava-33.jar!/com/google/common/base/Strings.class";
    await virtualDocument(jar);
    expect(relativePath("/p", jar)).toBe("/Users/me/My Libs/guava-33.jar › Strings.class");
    expect(relativeDir("/p", jar)).toBe("/Users/me/My Libs/guava-33.jar");
  });
});
