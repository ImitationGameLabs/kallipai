// Pins the dropdown menu data contract: the two disclosure ids, both
// locale label sets, the pinned item counts, and the "#" placeholder
// treatment every item carries until its section lands.
import assert from "node:assert/strict";
import { headerMenus } from "./header-menus.ts";

Deno.test("headerMenus exposes the two disclosure ids", () => {
  assert.deepEqual(
    headerMenus(false).map((menu) => menu.id),
    ["products", "solutions"],
  );
  assert.deepEqual(
    headerMenus(true).map((menu) => menu.id),
    ["products", "solutions"],
  );
});

Deno.test("headerMenus labels are locale-switched", () => {
  const [enProducts, enSolutions] = headerMenus(false);
  assert.equal(enProducts.label, "Products");
  assert.equal(enSolutions.label, "Solutions");
  const [zhProducts, zhSolutions] = headerMenus(true);
  assert.equal(zhProducts.label, "产品");
  assert.equal(zhSolutions.label, "解决方案");
  assert.equal(headerMenus(false)[0].items[0].label, "Agent Harness");
  assert.equal(headerMenus(true)[0].items[0].label, "智能体框架");
  assert.equal(headerMenus(false)[0].items[1].label, "Agent Platform");
  assert.equal(headerMenus(true)[0].items[1].label, "智能体平台");
  assert.equal(headerMenus(false)[1].items[0].label, "Private deployment");
  assert.equal(headerMenus(true)[1].items[0].label, "私有化部署");
  assert.equal(headerMenus(false)[1].items[1].label, "Vertical integration");
  assert.equal(headerMenus(true)[1].items[1].label, "垂直场景接入");
});

Deno.test("headerMenus carries the pinned item counts", () => {
  for (const isZh of [false, true]) {
    const [products, solutions] = headerMenus(isZh);
    assert.equal(products.items.length, 2, `products (${isZh})`);
    assert.equal(solutions.items.length, 2, `solutions (${isZh})`);
  }
});

Deno.test("headerMenus items share the # placeholder treatment", () => {
  for (const isZh of [false, true]) {
    for (const menu of headerMenus(isZh)) {
      for (const item of menu.items) {
        assert.equal(item.href, "#", `${menu.id}: ${item.label}`);
      }
    }
  }
});
