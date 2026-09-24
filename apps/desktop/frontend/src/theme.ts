/**
 * 界面主题:跟随系统 / 浅色 / 深色。
 *
 * 生效结果写在 `<html data-dark="true|false">` 上(app.css 的暗色规则全部
 * 以该属性为前提),同时把选择缓存在 localStorage —— 启动早期(profile.json
 * 还没读完时)也能立刻用对配色,避免"先白后黑"的闪屏。
 */

export type ThemeMode = "system" | "light" | "dark";

const STORAGE_KEY = "fq-theme";

/** 系统当前是否偏好深色。 */
export function systemPrefersDark(): boolean {
  try {
    return window.matchMedia("(prefers-color-scheme: dark)").matches;
  } catch {
    return false;
  }
}

/** 某个主题模式在当前系统下是否应呈现为深色。 */
export function resolvesDark(mode: ThemeMode): boolean {
  return mode === "dark" || (mode === "system" && systemPrefersDark());
}

/** 应用主题(写属性 + 缓存)。 */
export function applyTheme(mode: ThemeMode): void {
  document.documentElement.dataset.dark = String(resolvesDark(mode));
  try {
    localStorage.setItem(STORAGE_KEY, mode);
  } catch {
    /* 隐私模式等场景忽略 */
  }
}

/** 启动早期读取本机缓存(随后会被 profile.json 里的设置覆盖)。 */
export function cachedTheme(): ThemeMode {
  try {
    const value = localStorage.getItem(STORAGE_KEY);
    if (value === "light" || value === "dark" || value === "system") return value;
  } catch {
    /* 忽略 */
  }
  return "system";
}

/** 订阅系统配色变化(仅"跟随系统"模式需要),返回取消订阅函数。 */
export function watchSystemTheme(onChange: () => void): () => void {
  try {
    const query = window.matchMedia("(prefers-color-scheme: dark)");
    query.addEventListener("change", onChange);
    return () => query.removeEventListener("change", onChange);
  } catch {
    return () => undefined;
  }
}
