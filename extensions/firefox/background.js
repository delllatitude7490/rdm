import { createBridge } from "./bridge.js";
const bridge = createBridge(browser);
// Register synchronously at the event page's top level, no polling or ports.
browser.runtime.onInstalled.addListener(bridge.installMenu);
browser.runtime.onMessage.addListener(bridge.onMessage);
browser.menus.onClicked.addListener(bridge.onMenu);
