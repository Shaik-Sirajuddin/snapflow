const W = 1280;
const H = 720;

const SVGS = {
  mcp: `<svg width="180" height="180" viewBox="0 0 180 180" fill="none" xmlns="http://www.w3.org/2000/svg"><path d="M18 84.8528L85.8822 16.9706C95.2548 7.59798 110.451 7.59798 119.823 16.9706C129.196 26.3431 129.196 41.5391 119.823 50.9117L68.5581 102.177" stroke="#FFFFFF" stroke-width="12" stroke-linecap="round"/><path d="M69.2652 101.47L119.823 50.9117C129.196 41.5391 144.392 41.5391 153.765 50.9117L154.118 51.2652C163.491 60.6378 163.491 75.8338 154.118 85.2063L92.7248 146.6C89.6006 149.724 89.6006 154.789 92.7248 157.913L105.331 170.52" stroke="#FFFFFF" stroke-width="12" stroke-linecap="round"/><path d="M102.853 33.9411L52.6482 84.1457C43.2756 93.5183 43.2756 108.714 52.6482 118.087C62.0208 127.459 77.2167 127.459 86.5893 118.087L136.794 67.8822" stroke="#FFFFFF" stroke-width="12" stroke-linecap="round"/></svg>`,
  n8n: `<svg width="576" height="160" viewBox="0 0 576 160" fill="none" xmlns="http://www.w3.org/2000/svg"><path fill-rule="evenodd" clip-rule="evenodd" d="M272 64C257.089 64 244.561 53.8018 241.008 40H204.331C196.51 40 189.835 45.6546 188.549 53.3696L187.234 61.2608C185.985 68.7531 182.195 75.2738 176.835 80C182.195 84.7262 185.985 91.2469 187.234 98.7392L188.549 106.63C189.835 114.345 196.51 120 204.331 120H209.008C212.561 106.198 225.089 96 240 96C257.673 96 272 110.327 272 128C272 145.673 257.673 160 240 160C225.089 160 212.56 149.802 209.008 136H204.331C188.688 136 175.338 124.691 172.766 109.261L171.451 101.37C170.165 93.6546 163.49 88 155.669 88H142.992C139.44 101.802 126.911 112 112 112C97.0893 112 84.5605 101.802 81.0081 88H62.9919C59.4395 101.802 46.9107 112 32 112C14.3269 112 0 97.6731 0 80C0 62.3269 14.3269 48 32 48C46.9107 48 59.4395 58.1982 62.9919 72H81.0081C84.5605 58.1982 97.0893 48 112 48C126.911 48 139.44 58.1982 142.992 72H155.669C163.49 72 170.165 66.3454 171.451 58.6304L172.766 50.7392C175.338 35.3092 188.688 24 204.331 24L241.008 24C244.56 10.1982 257.089 0 272 0C289.673 0 304 14.3269 304 32C304 49.6731 289.673 64 272 64ZM272 48C280.837 48 288 40.8366 288 32C288 23.1634 280.837 16 272 16C263.163 16 256 23.1634 256 32C256 40.8366 263.163 48 272 48ZM32 96C40.8366 96 48 88.8366 48 80C48 71.1634 40.8366 64 32 64C23.1634 64 16 71.1634 16 80C16 88.8366 23.1634 96 32 96ZM128 80C128 88.8366 120.837 96 112 96C103.163 96 96 88.8366 96 80C96 71.1634 103.163 64 112 64C120.837 64 128 71.1634 128 80ZM256 128C256 136.837 248.837 144 240 144C231.163 144 224 136.837 224 128C224 119.163 231.163 112 240 112C248.837 112 256 119.163 256 128Z" fill="#EA4B71"/><path fill-rule="evenodd" clip-rule="evenodd" d="M480.023 69.1834V68.421C485.605 65.6252 491.188 60.7962 491.188 51.2651C491.188 37.5405 479.896 29.2803 464.29 29.2803C448.304 29.2803 436.885 38.0488 436.885 51.5193C436.885 60.6691 442.214 65.6252 448.05 68.421V69.1834C441.579 71.4709 433.84 78.3332 433.84 89.7704C433.84 103.622 445.259 113.28 464.163 113.28C483.068 113.28 494.106 103.622 494.106 89.7704C494.106 78.3332 486.494 71.598 480.023 69.1834ZM464.163 40.9717C470.507 40.9717 475.202 45.0382 475.202 51.9005C475.202 58.7629 470.38 62.8294 464.163 62.8294C457.946 62.8294 452.744 58.7629 452.744 51.9005C452.744 44.9111 457.693 40.9717 464.163 40.9717ZM464.163 101.081C456.804 101.081 450.841 96.3786 450.841 88.3726C450.841 81.129 455.789 75.6645 464.036 75.6645C472.156 75.6645 477.105 81.0019 477.105 88.6267C477.105 96.3786 471.395 101.081 464.163 101.081Z" fill="#FFFFFF"/><path d="M513.68 112.009H529.92V77.5707C529.92 66.2606 536.771 61.3045 544.511 61.3045C552.123 61.3045 558.087 66.3877 558.087 76.8083V112.009H574.327V73.5042C574.327 56.8567 564.684 47.1986 549.586 47.1986C540.07 47.1986 534.741 51.011 530.935 55.9671H529.92L528.524 48.4694H513.68V112.009Z" fill="#FFFFFF"/><path d="M369.92 112.009H353.68V48.4694H368.524L369.92 55.9671H370.935C374.741 51.011 380.07 47.1986 389.586 47.1986C404.684 47.1986 414.327 56.8567 414.327 73.5042V112.009H398.087V76.8083C398.087 66.3877 392.123 61.3045 384.511 61.3045C376.771 61.3045 369.92 66.2606 369.92 77.5707V112.009Z" fill="#FFFFFF"/></svg>`,
  codex: `<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="#FFFFFF" fill-rule="evenodd"><path clip-rule="evenodd" d="M8.086.457a6.105 6.105 0 013.046-.415c1.333.153 2.521.72 3.564 1.7a.117.117 0 00.107.029c1.408-.346 2.762-.224 4.061.366l.063.03.154.076c1.357.703 2.33 1.77 2.918 3.198.278.679.418 1.388.421 2.126a5.655 5.655 0 01-.18 1.631.167.167 0 00.04.155 5.982 5.982 0 011.578 2.891c.385 1.901-.01 3.615-1.183 5.14l-.182.22a6.063 6.063 0 01-2.934 1.851.162.162 0 00-.108.102c-.255.736-.511 1.364-.987 1.992-1.199 1.582-2.962 2.462-4.948 2.451-1.583-.008-2.986-.587-4.21-1.736a.145.145 0 00-.14-.032c-.518.167-1.04.191-1.604.185a5.924 5.924 0 01-2.595-.622 6.058 6.058 0 01-2.146-1.781c-.203-.269-.404-.522-.551-.821a7.74 7.74 0 01-.495-1.283 6.11 6.11 0 01-.017-3.064.166.166 0 00.008-.074.115.115 0 00-.037-.064 5.958 5.958 0 01-1.38-2.202 5.196 5.196 0 01-.333-1.589 6.915 6.915 0 01.188-2.132c.45-1.484 1.309-2.648 2.577-3.493.282-.188.55-.334.802-.438.286-.12.573-.22.861-.304a.129.129 0 00.087-.087A6.016 6.016 0 015.635 2.31C6.315 1.464 7.132.846 8.086.457zm-.804 7.85a.848.848 0 00-1.473.842l1.694 2.965-1.688 2.848a.849.849 0 001.46.864l1.94-3.272a.849.849 0 00.007-.854l-1.94-3.393zm5.446 6.24a.849.849 0 000 1.695h4.848a.849.849 0 000-1.696h-4.848z"/></svg>`,
  claude: `<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100" viewBox="0 0 100 100" fill="#DA7757"><path d="m19.6 66.5 19.7-11 .3-1-.3-.5h-1l-3.3-.2-11.2-.3L14 53l-9.5-.5-2.4-.5L0 49l.2-1.5 2-1.3 2.9.2 6.3.5 9.5.6 6.9.4L38 49.1h1.6l.2-.7-.5-.4-.4-.4L29 41l-10.6-7-5.6-4.1-3-2-1.5-2-.6-4.2 2.7-3 3.7.3.9.2 3.7 2.9 8 6.1L37 36l1.5 1.2.6-.4.1-.3-.7-1.1L33 25l-6-10.4-2.7-4.3-.7-2.6c-.3-1-.4-2-.4-3l3-4.2L28 0l4.2.6L33.8 2l2.6 6 4.1 9.3L47 29.9l2 3.8 1 3.4.3 1h.7v-.5l.5-7.2 1-8.7 1-11.2.3-3.2 1.6-3.8 3-2L61 2.6l2 2.9-.3 1.8-1.1 7.7L59 27.1l-1.5 8.2h.9l1-1.1 4.1-5.4 6.9-8.6 3-3.5L77 13l2.3-1.8h4.3l3.1 4.7-1.4 4.9-4.4 5.6-3.7 4.7-5.3 7.1-3.2 5.7.3.4h.7l12-2.6 6.4-1.1 7.6-1.3 3.5 1.6.4 1.6-1.4 3.4-8.2 2-9.6 2-14.3 3.3-.2.1.2.3 6.4.6 2.8.2h6.8l12.6 1 3.3 2 1.9 2.7-.3 2-5.1 2.6-6.8-1.6-16-3.8-5.4-1.3h-.8v.4l4.6 4.5 8.3 7.5L89 80.1l.5 2.4-1.3 2-1.4-.2-9.2-7-3.6-3-8-6.8h-.5v.7l1.8 2.7 9.8 14.7.5 4.5-.7 1.4-2.6 1-2.7-.6-5.8-8-6-9-4.7-8.2-.5.4-2.9 30.2-1.3 1.5-3 1.2-2.5-2-1.4-3 1.4-6.2 1.6-8 1.3-6.4 1.2-7.9.7-2.6v-.2H49L43 72l-9 12.3-7.2 7.6-1.7.7-3-1.5.3-2.8L24 86l10-12.8 6-7.9 4-4.6-.1-.5h-.3L17.2 77.4l-4.7.6-2-2 .2-3 1-1 8-5.5Z"/></svg>`
};

function fontExact(name) {
  const all = penpot.fonts.findAllByName(name) || [];
  return all.find((f) => f.name === name) || penpot.fonts.findByName(name);
}

function applyFont(text, family, weight) {
  const font = fontExact(family);
  if (!font) return false;
  const variant = (font.variants || []).find(
    (v) => String(v.fontWeight) === String(weight) && (v.fontStyle === "normal" || !v.fontStyle)
  );
  font.applyToText(text, variant);
  return true;
}

function styleText(text, opts) {
  text.characters = opts.characters;
  applyFont(text, opts.family, opts.weight);
  text.fontSize = String(opts.size);
  text.fills = [{ fillColor: opts.color, fillOpacity: opts.opacity == null ? 1 : opts.opacity }];
  if (opts.letterSpacing != null) text.letterSpacing = String(opts.letterSpacing);
  if (opts.transform) text.textTransform = opts.transform;
  text.align = opts.align || "center";
  text.growType = "auto-width";
  if (opts.shadow) text.shadows = [opts.shadow];
}

function glowShadow(color, blur, opacity) {
  return {
    style: "drop-shadow",
    offsetX: 0,
    offsetY: 0,
    blur,
    spread: 0,
    color: { color, opacity }
  };
}

function makeBoard(name, w, h) {
  const b = penpot.createBoard();
  b.name = name;
  b.resize(w, h);
  return b;
}

function flexRow(board, gap) {
  const flex = board.addFlexLayout();
  flex.dir = "row";
  flex.alignItems = "center";
  flex.justifyContent = "center";
  flex.columnGap = gap;
  board.horizontalSizing = "fix";
  board.verticalSizing = "fix";
  return flex;
}

function importSvg(svg, name, w, h) {
  const shape = penpot.createShapeFromSvg(svg);
  if (!shape) throw new Error("Failed to import SVG: " + name);
  shape.name = name;
  shape.resize(w, h);
  return shape;
}

function glassCard(name, w, h, radius) {
  const card = makeBoard(name, w, h);
  card.borderRadius = radius;
  card.fills = [
    {
      fillColorGradient: {
        type: "linear",
        startX: 0.5,
        startY: 0,
        endX: 0.5,
        endY: 1,
        width: 1,
        stops: [
          { color: "#1A3F86", opacity: 0.42, offset: 0 },
          { color: "#0B1A3A", opacity: 0.62, offset: 1 }
        ]
      },
      fillOpacity: 1
    }
  ];
  card.strokes = [
    {
      strokeColor: "#9ED2FF",
      strokeOpacity: 0.28,
      strokeWidth: 1.5,
      strokeAlignment: "inner",
      strokeStyle: "solid"
    }
  ];
  card.shadows = [
    glowShadow("#2F8CFF", 36, 0.38),
    { style: "drop-shadow", offsetX: 0, offsetY: 18, blur: 28, spread: -4, color: { color: "#020817", opacity: 0.45 } }
  ];
  flexRow(card, 18);
  card.flex.horizontalPadding = 28;
  card.flex.verticalPadding = 22;
  return card;
}

// Remove a previous attempt if we re-run
const existing = penpotUtils.findShape((s) => s.name === "YT Thumb / Build MCP Server / Blue Backlight");
if (existing) existing.remove();

const root = makeBoard("YT Thumb / Build MCP Server / Blue Backlight", W, H);
root.clipContent = true;
root.x = 80;
root.y = 80;
root.fills = [
  {
    fillColorGradient: {
      type: "linear",
      startX: 0.12,
      startY: 0,
      endX: 0.88,
      endY: 1,
      width: 1,
      stops: [
        { color: "#071633", opacity: 1, offset: 0 },
        { color: "#061028", opacity: 1, offset: 0.48 },
        { color: "#0A2360", opacity: 1, offset: 1 }
      ]
    }
  }
];
root.shadows = [
  { style: "drop-shadow", offsetX: 0, offsetY: 24, blur: 48, spread: 0, color: { color: "#041028", opacity: 0.5 } }
];

function ellipseGlow(name, w, h, x, y, stops, blur) {
  const e = penpot.createEllipse();
  e.name = name;
  e.resize(w, h);
  e.fills = [
    {
      fillColorGradient: {
        type: "radial",
        startX: 0.5,
        startY: 0.5,
        endX: 0.5,
        endY: 1,
        width: 1,
        stops
      }
    }
  ];
  e.blur = { type: "layer-blur", value: blur };
  root.appendChild(e);
  penpotUtils.setParentXY(e, x, y);
  return e;
}

ellipseGlow(
  "Backlight / far bloom",
  1180,
  760,
  50,
  -40,
  [
    { color: "#3AA0FF", opacity: 0.55, offset: 0 },
    { color: "#1554C8", opacity: 0.18, offset: 0.42 },
    { color: "#061028", opacity: 0, offset: 1 }
  ],
  70
);

ellipseGlow(
  "Backlight / core",
  720,
  420,
  280,
  70,
  [
    { color: "#B7E3FF", opacity: 0.72, offset: 0 },
    { color: "#3B9BFF", opacity: 0.28, offset: 0.4 },
    { color: "#061028", opacity: 0, offset: 1 }
  ],
  46
);

ellipseGlow(
  "Backlight / title wash",
  980,
  280,
  150,
  430,
  [
    { color: "#2B7BFF", opacity: 0.32, offset: 0 },
    { color: "#061028", opacity: 0, offset: 1 }
  ],
  40
);

const vignette = penpot.createRectangle();
vignette.name = "Vignette";
vignette.resize(W, H);
vignette.fills = [
  {
    fillColorGradient: {
      type: "radial",
      startX: 0.5,
      startY: 0.45,
      endX: 0.5,
      endY: 1.05,
      width: 1,
      stops: [
        { color: "#000000", opacity: 0, offset: 0.58 },
        { color: "#020617", opacity: 0.55, offset: 1 }
      ]
    }
  }
];
root.appendChild(vignette);
penpotUtils.setParentXY(vignette, 0, 0);

const content = makeBoard("Content", W, H);
content.fills = [];
content.strokes = [];
root.appendChild(content);
penpotUtils.setParentXY(content, 0, 0);
const contentFlex = content.addFlexLayout();
contentFlex.dir = "column";
contentFlex.alignItems = "center";
contentFlex.justifyContent = "center";
contentFlex.rowGap = 26;
content.flex.topPadding = 52;
content.flex.bottomPadding = 44;
content.flex.horizontalPadding = 64;
content.horizontalSizing = "fix";
content.verticalSizing = "fix";

const hero = makeBoard("Hero / MCP + n8n", 1152, 228);
hero.fills = [];
content.appendChild(hero);
flexRow(hero, 28);

const mcpCard = glassCard("MCP lockup", 500, 204, 32);
hero.appendChild(mcpCard);

const mcpLogo = importSvg(SVGS.mcp, "MCP mark", 118, 118);
mcpLogo.shadows = [glowShadow("#7EC8FF", 22, 0.55)];
mcpCard.appendChild(mcpLogo);

const mcpLabel = penpot.createText("MCP");
mcpLabel.name = "MCP wordmark";
styleText(mcpLabel, {
  characters: "MCP",
  family: "Unbounded",
  weight: "800",
  size: 56,
  color: "#F4F8FF",
  letterSpacing: 1.2,
  shadow: glowShadow("#4EA2FF", 16, 0.45)
});
mcpCard.appendChild(mcpLabel);

const plusWrap = makeBoard("Plus", 48, 48);
plusWrap.fills = [];
hero.appendChild(plusWrap);
const plus = penpot.createText("+");
plus.name = "Plus mark";
styleText(plus, {
  characters: "+",
  family: "Outfit",
  weight: "500",
  size: 42,
  color: "#9EC8FF",
  opacity: 0.9
});
plusWrap.appendChild(plus);
plusWrap.addFlexLayout();
plusWrap.flex.dir = "row";
plusWrap.flex.alignItems = "center";
plusWrap.flex.justifyContent = "center";
plusWrap.horizontalSizing = "fix";
plusWrap.verticalSizing = "fix";

const n8nCard = glassCard("n8n lockup", 500, 204, 32);
hero.appendChild(n8nCard);
const n8nLogo = importSvg(SVGS.n8n, "n8n official lockup", 412, 114);
n8nLogo.shadows = [glowShadow("#EA4B71", 18, 0.28), glowShadow("#6EB6FF", 16, 0.22)];
n8nCard.appendChild(n8nLogo);

const clients = makeBoard("Clients / Codex + Claude", 760, 124);
clients.fills = [];
content.appendChild(clients);
flexRow(clients, 22);

function chip(name, w) {
  const c = glassCard(name, w, 112, 24);
  c.flex.horizontalPadding = 22;
  c.flex.verticalPadding = 16;
  c.flex.columnGap = 14;
  return c;
}

const codexChip = chip("Codex chip", 318);
clients.appendChild(codexChip);
const codexLogo = importSvg(SVGS.codex, "Codex mark", 46, 46);
codexLogo.shadows = [glowShadow("#FFFFFF", 12, 0.28)];
codexChip.appendChild(codexLogo);
const codexLabel = penpot.createText("Codex");
codexLabel.name = "Codex label";
styleText(codexLabel, {
  characters: "Codex",
  family: "Inter",
  weight: "600",
  size: 28,
  color: "#EAF2FF"
});
codexChip.appendChild(codexLabel);

const claudeChip = chip("Claude chip", 318);
clients.appendChild(claudeChip);
const claudeLogo = importSvg(SVGS.claude, "Claude mark", 50, 50);
claudeLogo.shadows = [glowShadow("#DA7757", 14, 0.4)];
claudeChip.appendChild(claudeLogo);
const claudeLabel = penpot.createText("Claude");
claudeLabel.name = "Claude label";
styleText(claudeLabel, {
  characters: "Claude",
  family: "Inter",
  weight: "600",
  size: 28,
  color: "#EAF2FF"
});
claudeChip.appendChild(claudeLabel);

const title = penpot.createText("Build MCP Server");
title.name = "Title";
styleText(title, {
  characters: "Build MCP Server",
  family: "Unbounded",
  weight: "800",
  size: 68,
  color: "#FFFFFF",
  letterSpacing: 0,
  shadow: glowShadow("#4EA2FF", 22, 0.55)
});
content.appendChild(title);

storage.thumbId = root.id;
storage.mcpLogoId = mcpLogo.id;
storage.n8nLogoId = n8nLogo.id;
storage.codexLogoId = codexLogo.id;
storage.claudeLogoId = claudeLogo.id;

return {
  id: root.id,
  structure: penpotUtils.shapeStructure(root, 3),
  imported: {
    mcp: { w: mcpLogo.width, h: mcpLogo.height },
    n8n: { w: n8nLogo.width, h: n8nLogo.height },
    codex: { w: codexLogo.width, h: codexLogo.height },
    claude: { w: claudeLogo.width, h: claudeLogo.height }
  }
};
