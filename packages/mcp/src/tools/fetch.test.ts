import { describe, it, expect } from "vitest";
import { __testing } from "./fetch";

const { isPrivateIp, assertPublicUrl } = __testing;

describe("fetch_url SSRF guard — isPrivateIp", () => {
  it("flags loopback, RFC1918, link-local / cloud-metadata, and CGNAT-edge ranges", () => {
    for (const ip of [
      "127.0.0.1",
      "10.0.0.5",
      "192.168.1.1",
      "172.16.0.1",
      "172.31.255.255",
      "169.254.169.254", // cloud metadata
      "0.0.0.0",
    ]) {
      expect(isPrivateIp(ip), ip).toBe(true);
    }
  });

  it("flags IPv6 loopback / link-local / unique-local and IPv4-mapped privates", () => {
    for (const ip of ["::1", "fe80::1", "fc00::1", "fd12:3456::1", "::ffff:10.0.0.1"]) {
      expect(isPrivateIp(ip), ip).toBe(true);
    }
  });

  it("allows public addresses", () => {
    for (const ip of ["8.8.8.8", "1.1.1.1", "172.15.0.1", "172.32.0.1", "2606:4700::1"]) {
      expect(isPrivateIp(ip), ip).toBe(false);
    }
  });
});

describe("fetch_url SSRF guard — assertPublicUrl", () => {
  it("rejects non-http(s) schemes", async () => {
    await expect(assertPublicUrl("file:///etc/passwd")).rejects.toThrow(/scheme/);
    await expect(assertPublicUrl("ftp://example.com")).rejects.toThrow(/scheme/);
  });

  it("rejects a literal private IP host", async () => {
    await expect(assertPublicUrl("http://169.254.169.254/latest/meta-data/")).rejects.toThrow(
      /private\/loopback/,
    );
    await expect(assertPublicUrl("http://127.0.0.1:8080/")).rejects.toThrow(/private\/loopback/);
  });

  it("rejects localhost (resolves to loopback)", async () => {
    await expect(assertPublicUrl("http://localhost/admin")).rejects.toThrow(/private\/loopback/);
  });

  it("rejects a malformed URL", async () => {
    await expect(assertPublicUrl("not a url")).rejects.toThrow(/invalid URL/);
  });
});
