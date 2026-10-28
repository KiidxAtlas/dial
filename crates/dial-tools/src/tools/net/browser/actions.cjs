// Page-side execution for the pointer, keyboard, element-query and conditional wait actions.
// Arguments are validated by actions.rs before this runs. Inlined into the driver by driver_source.
const PAGE_ACTIONS = new Set(['scroll','dblclick','drag','click_at','mouse_move','mouse_down','mouse_up','wheel','key_down','key_up','highlight','text','html','value','attr','count','box','is_visible','is_enabled','is_checked','styles']);
const FRAME_ACTIONS = new Set(['scroll','dblclick','drag','highlight','text','html','value','attr','count','box','is_visible','is_enabled','is_checked','styles']);
const TEXT_LIMIT = 20000;
const DEFAULT_HIGHLIGHT_MS = 2000;
const DEFAULT_STYLES = ['font-family','font-size','font-weight','color','background-color','display','visibility','opacity','position','z-index','width','height','margin','padding'];

function waitCondition(request) {
  if (request.url != null) return 'url';
  if (request.text != null) return 'text';
  if (request.response != null) return 'response';
  return null;
}

function handles(request) {
  if (PAGE_ACTIONS.has(request.action)) return true;
  return request.action === 'wait' && waitCondition(request) !== null;
}

// Runs in the page realm: removes earlier highlights, paints a fixed overlay over the element,
// and removes it after `ms`. Must stay self-contained because Playwright serializes it.
function paintHighlight(element, ms) {
  for (const old of document.querySelectorAll('div[data-dial-highlight]')) old.remove();
  const rect = element.getBoundingClientRect();
  const overlay = document.createElement('div');
  overlay.setAttribute('data-dial-highlight', '');
  Object.assign(overlay.style, {
    position: 'fixed', left: rect.left + 'px', top: rect.top + 'px',
    width: rect.width + 'px', height: rect.height + 'px', boxSizing: 'border-box',
    outline: '2px solid #ff3b30', background: 'rgba(255,59,48,0.15)',
    pointerEvents: 'none', zIndex: '2147483647',
  });
  document.documentElement.appendChild(overlay);
  setTimeout(() => overlay.remove(), ms);
  return { left: rect.left, top: rect.top, width: rect.width, height: rect.height };
}

async function run(request, ctx) {
  const { page, scope, frameId, cut, clip } = ctx;
  const action = request.action;
  const condition = action === 'wait' ? waitCondition(request) : null;
  const frameOk = action === 'wait' ? condition === 'text' : FRAME_ACTIONS.has(action);
  if (request.frame !== undefined && !frameOk) throw new Error(`frame is not supported for action=${action}`);
  const done = fields => ({ tab: request.tab, frame: frameId, action, completed: true, ...fields });
  const bounded = (key, value) => ({ [key]: cut(value, TEXT_LIMIT), truncated: value.length > TEXT_LIMIT });
  const sel = request.selector ?? null;
  const button = request.button ?? 'left';
  switch (action) {
    case 'scroll': {
      const delta = { left: request.dx ?? 0, top: request.dy ?? 0 };
      const position = sel
        ? await scope.locator(sel).evaluate((el, d) => {
            el.scrollBy({ left: d.left, top: d.top, behavior: 'instant' });
            return { left: el.scrollLeft, top: el.scrollTop };
          }, delta)
        : await scope.evaluate(d => {
            window.scrollBy({ left: d.left, top: d.top, behavior: 'instant' });
            return { left: window.scrollX, top: window.scrollY };
          }, delta);
      return done({ selector: sel, position });
    }
    case 'dblclick': await scope.locator(sel).dblclick(); return done({ selector: sel });
    case 'drag': {
      await scope.locator(sel).dragTo(scope.locator(request.target));
      return done({ selector: sel, target: request.target });
    }
    case 'click_at': await page.mouse.click(request.x, request.y, { button }); return done({ x: request.x, y: request.y, button });
    case 'mouse_move': await page.mouse.move(request.x, request.y); return done({ x: request.x, y: request.y });
    case 'mouse_down': await page.mouse.down({ button }); return done({ button });
    case 'mouse_up': await page.mouse.up({ button }); return done({ button });
    case 'wheel': {
      const dx = request.dx ?? 0, dy = request.dy ?? 0;
      await page.mouse.wheel(dx, dy);
      return done({ dx, dy });
    }
    case 'key_down': await page.keyboard.down(request.key); return done({ key: request.key });
    case 'key_up': await page.keyboard.up(request.key); return done({ key: request.key });
    case 'highlight': {
      const box = await scope.locator(sel).first().evaluate(paintHighlight, request.duration ?? DEFAULT_HIGHLIGHT_MS);
      return done({ selector: sel, box });
    }
    case 'text': {
      const text = await scope.locator(sel ?? 'body').first().innerText();
      return done({ selector: sel, ...bounded('text', text) });
    }
    case 'html': {
      const html = await scope.locator(sel ?? 'html').first().innerHTML();
      return done({ selector: sel, ...bounded('html', html) });
    }
    case 'value': {
      const value = await scope.locator(sel).first().evaluate(el => {
        if (!('value' in el)) throw new Error('element has no value property');
        return String(el.value);
      });
      return done({ selector: sel, ...bounded('value', value) });
    }
    case 'attr': {
      const value = await scope.locator(sel).first().getAttribute(request.name);
      if (value === null) return done({ selector: sel, name: request.name, value: null, truncated: false });
      return done({ selector: sel, name: request.name, ...bounded('value', value) });
    }
    case 'count': return done({ selector: sel, count: await scope.locator(sel).count() });
    case 'box': return done({ selector: sel, box: await scope.locator(sel).first().boundingBox() });
    case 'is_visible': return done({ selector: sel, visible: await scope.locator(sel).first().isVisible() });
    case 'is_enabled': return done({ selector: sel, enabled: await scope.locator(sel).first().isEnabled() });
    case 'is_checked': return done({ selector: sel, checked: await scope.locator(sel).first().isChecked() });
    case 'styles': {
      const props = request.props ?? DEFAULT_STYLES;
      const styles = await scope.locator(sel).first().evaluate((el, names) => {
        const computed = getComputedStyle(el);
        const out = {};
        for (const name of names) out[name] = computed.getPropertyValue(name);
        return out;
      }, props);
      return done({ selector: sel, styles });
    }
    case 'wait': {
      if (condition === 'url') {
        await page.waitForFunction(fragment => location.href.includes(fragment), request.url, { timeout: request.timeout_ms });
        return done({ condition, url: clip(request.url), current_url: clip(page.url()) });
      }
      if (condition === 'text') {
        await scope.waitForFunction(([selector, fragment]) => {
          const element = selector ? document.querySelector(selector) : document.body;
          return Boolean(element) && (element.innerText || '').includes(fragment);
        }, [sel, request.text], { timeout: request.timeout_ms });
        return done({ condition, selector: sel });
      }
      const response = await page.waitForResponse(res => res.url().includes(request.response), { timeout: request.timeout_ms });
      return done({ condition, response_url: clip(response.url()), status: response.status() });
    }
    default: throw new Error(`unsupported browser action ${action}`);
  }
}

module.exports = { handles, run };
