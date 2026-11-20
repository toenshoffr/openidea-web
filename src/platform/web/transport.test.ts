import { describe, expect, it } from "vitest";
import { Transport } from "./transport";

describe("Transport.address", () => {
  it("is relative to the page, so a proxy prefix is kept", () => {
    expect(Transport.address("http://localhost:8080/")).toBe("ws://localhost:8080/ws");
    expect(Transport.address("https://lab.example.com/user/proj/proxy/8080/")).toBe(
      "wss://lab.example.com/user/proj/proxy/8080/ws",
    );
  });
});
