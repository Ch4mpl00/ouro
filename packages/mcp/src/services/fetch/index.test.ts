import dns from "node:dns/promises";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { assertPublicUrl, createGuardedFetch, isPrivateIp } from ".";

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

// A local server stands in for the internet: the guard is told that only
// 127.0.0.1 is public, so ::1 and anything else play the internal service a
// crafted redirect points at.
describe("guarded fetch", () => {
  let server: http.Server;
  let port = 0;
  const onlyLoopbackV4IsPublic = (ip: string) => ip !== "127.0.0.1";

  beforeAll(async () => {
    server = http.createServer((req, res) => {
      const redirect = (location: string) => res.writeHead(302, { location }).end();
      if (req.url === "/ok") return void res.end("hello");
      if (req.url === "/relative") return redirect("/ok");
      if (req.url === "/to-ipv6") return redirect(`http://[::1]:${port}/ok`);
      if (req.url === "/to-name") return redirect(`http://localhost:${port}/ok`);
      if (req.url === "/to-file") return redirect("file:///etc/passwd");
      if (req.url === "/loop") return redirect("/loop");
      res.writeHead(404).end();
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    port = (server.address() as AddressInfo).port;
  });
  afterAll(async () => {
    await new Promise((resolve) => server.close(resolve));
  });

  it("follows a redirect to an allowed address and reports the final URL", async () => {
    const guarded = createGuardedFetch({ isPrivate: onlyLoopbackV4IsPublic });

    const { url, response } = await guarded(`http://127.0.0.1:${port}/relative`);

    expect(url).toBe(`http://127.0.0.1:${port}/ok`);
    expect(await response.text()).toBe("hello");
  });

  // The TS guard used to check only the first URL; fetch() then followed
  // redirects anywhere.
  it("refuses a redirect to a literal private IP", async () => {
    const guarded = createGuardedFetch({ isPrivate: onlyLoopbackV4IsPublic });

    await expect(guarded(`http://127.0.0.1:${port}/to-ipv6`)).rejects.toThrow(/private\/loopback.*::1/);
  });

  it("refuses a redirect to a name that resolves to a private address", async () => {
    const guarded = createGuardedFetch({ isPrivate: onlyLoopbackV4IsPublic });
    const resolved = await dns.lookup("localhost", { all: true });
    // Meaningful only where localhost also resolves to something besides 127.0.0.1.
    if (resolved.every((a) => a.address === "127.0.0.1")) return;

    await expect(guarded(`http://127.0.0.1:${port}/to-name`)).rejects.toThrow(/private\/loopback/);
  });

  it("refuses a redirect to a non-http scheme and a redirect loop", async () => {
    const guarded = createGuardedFetch({ isPrivate: onlyLoopbackV4IsPublic, maxRedirects: 3 });

    await expect(guarded(`http://127.0.0.1:${port}/to-file`)).rejects.toThrow(/scheme/);
    await expect(guarded(`http://127.0.0.1:${port}/loop`)).rejects.toThrow(/too many redirects/);
  });

  // DNS rebinding: the name was public when validated and private by the
  // time the connection was made. The check at connect time catches it.
  it("re-checks the address at connect time", async () => {
    const atValidation = (await dns.lookup("localhost", { all: true })).length;
    let checks = 0;
    const guarded = createGuardedFetch({ isPrivate: () => ++checks > atValidation });

    await expect(guarded(`http://localhost:${port}/ok`)).rejects.toThrow(/private\/loopback/);
    expect(checks).toBeGreaterThan(atValidation);
  });
});
