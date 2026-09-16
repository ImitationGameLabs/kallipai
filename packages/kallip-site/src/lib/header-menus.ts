// Header dropdown menu data. Placeholder items point at "#" until their
// sections land, sharing the operator-retained placeholder treatment of
// News; real options later mean editing this data, not the component.
export interface HeaderMenuItem {
  label: string;
  href: string;
}

export interface HeaderMenu {
  id: string;
  label: string;
  items: HeaderMenuItem[];
}

export function headerMenus(isZh: boolean): HeaderMenu[] {
  return [
    {
      id: "products",
      label: isZh ? "产品" : "Products",
      items: [
        { label: isZh ? "智能体框架" : "Agent Harness", href: "#" },
        { label: isZh ? "智能体平台" : "Agent Platform", href: "#" },
      ],
    },
    {
      id: "solutions",
      label: isZh ? "解决方案" : "Solutions",
      items: [
        { label: isZh ? "私有化部署" : "Private deployment", href: "#" },
        { label: isZh ? "垂直场景接入" : "Vertical integration", href: "#" },
      ],
    },
  ];
}
