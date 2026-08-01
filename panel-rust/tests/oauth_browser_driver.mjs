// Drives one real MCP OAuth 2.1 `authorization_url` through an actual
// Chrome browser (Playwright, using the system's installed `google-
// chrome` binary via `channel: "chrome"` -- no separate bundled browser
// download needed) -- clicks the real "Approve" button on
// `oauth_stub_server.py`'s HTML consent page, exactly as a human would.
// Used by `live_oauth_browser_e2e.sh`; not meant to be run standalone
// against anything other than that stub server's page shape.
import { chromium } from "playwright";

const authorizationUrl = process.argv[2];
if (!authorizationUrl) {
  console.error("usage: node oauth_browser_driver.mjs <authorization_url>");
  process.exit(1);
}

const browser = await chromium.launch({ channel: "chrome", headless: true });
const page = await browser.newPage();

console.log(`[browser] navigating to ${authorizationUrl}`);
await page.goto(authorizationUrl);
console.log(`[browser] page title: ${await page.title()}`);

const approveButton = page.locator("#approve-button");
await approveButton.waitFor({ state: "visible", timeout: 5000 });
console.log("[browser] clicking Approve button");

// The click submits a form whose action 302-redirects to acpx's own
// loopback listener (127.0.0.1:<port>/callback) -- that redirect target
// isn't itself a real site (the loopback listener just serves a plain
// HTML acknowledgement), so wait for the navigation to settle rather
// than for a specific selector on the far side.
await Promise.all([
  page.waitForNavigation({ waitUntil: "load", timeout: 5000 }).catch(() => {}),
  approveButton.click(),
]);

console.log(`[browser] final URL: ${page.url()}`);
console.log(`[browser] final page text: ${(await page.textContent("body"))?.trim()}`);

await browser.close();
console.log("[browser] done");
