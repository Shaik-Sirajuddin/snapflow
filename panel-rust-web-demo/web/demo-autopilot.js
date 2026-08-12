const pause = (milliseconds) => new Promise((resolve) => window.setTimeout(resolve, milliseconds));

function canvasPoint(canvas, x, y) {
  const bounds = canvas.getBoundingClientRect();
  return {
    clientX: bounds.left + x,
    clientY: bounds.top + y,
  };
}

function clickCanvas(canvas, x, y) {
  const point = canvasPoint(canvas, x, y);
  for (const type of ["pointermove", "pointerdown", "pointerup"]) {
    canvas.dispatchEvent(
      new PointerEvent(type, {
        bubbles: true,
        pointerId: 1,
        pointerType: "mouse",
        button: 0,
        buttons: type === "pointerup" ? 0 : 1,
        ...point,
      }),
    );
  }
  canvas.dispatchEvent(new MouseEvent("click", { bubbles: true, button: 0, ...point }));
}

function typeIntoCanvas(canvas, text) {
  for (const key of text) {
    canvas.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, key }));
    canvas.dispatchEvent(new KeyboardEvent("keypress", { bubbles: true, key }));
    canvas.dispatchEvent(new KeyboardEvent("keyup", { bubbles: true, key }));
  }
}

export async function runMarketingAutopilot({ canvas }) {
  const params = new URLSearchParams(window.location.search);
  const forceAutoplay = params.get("autoplay") === "1";
  if (!canvas || params.get("autoplay") === "0" || (navigator.webdriver && !forceAutoplay)) return;

  const prompt = "Keep the dialogue clear as the final shot fades to black.";
  const threadSequence = [0, 1, 2, 3, 4];

  await pause(400);
  for (const threadIndex of threadSequence) {
    clickCanvas(canvas, 70, 145 + threadIndex * 54);
    await pause(700);
  }

  const bounds = canvas.getBoundingClientRect();
  clickCanvas(canvas, bounds.width * 0.58, bounds.height - 56);
  for (let end = 1; end <= prompt.length; end += 1) {
    typeIntoCanvas(canvas, prompt[end - 1]);
    await pause(24);
  }

  await pause(350);
  canvas.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, key: "Enter" }));
  canvas.dispatchEvent(new KeyboardEvent("keyup", { bubbles: true, key: "Enter" }));
}
