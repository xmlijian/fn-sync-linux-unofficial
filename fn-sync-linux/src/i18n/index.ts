// 官方语言包（自 0.2.3 客户端提取），key 结构保持一致。
// 品牌占位符替换与官方 brandReplacements 相同。
import zh from "./zh.json";
import en from "./en.json";
import ja from "./ja.json";

export const locales = { zh, en, ja } as const;

export type LocaleKey = keyof typeof locales;
export const localeNames: Record<LocaleKey, string> = {
  zh: "简体中文",
  en: "English",
  ja: "日本語",
};

const dicts: Record<string, Record<string, string>> = {
  zh: zh as Record<string, string>,
  en: en as Record<string, string>,
  ja: ja as Record<string, string>,
};

export const brandReplacements: Record<string, string> = {
  __APP_NAME__: "飞牛同步",
  __BRAND__: "FN",
  __BRAND_CONNECT__: "FN Connect",
  __BRAND_ID__: "FN ID",
};

function applyBrand(s: string): string {
  let out = s;
  for (const [k, v] of Object.entries(brandReplacements)) {
    out = out.split(k).join(v);
  }
  return out;
}

export function setLocale(locale: LocaleKey) {
  localStorage.setItem("language", locale);
}

export function getLocale(): LocaleKey {
  const saved = localStorage.getItem("language");
  if (saved && saved in locales) return saved as LocaleKey;
  const nav = navigator.language.toLowerCase();
  if (nav.startsWith("zh")) return "zh";
  if (nav.startsWith("ja")) return "ja";
  return "en";
}

type Vars = Record<string, string | number>;

export function translate(key: string, fallback = "", vars?: Vars): string {
  const locale = getLocale();
  const dict = dicts[locale] ?? dicts.en;
  let text = dict[key] ?? dicts.en[key] ?? fallback;
  if (!text) text = key;
  if (vars) {
    for (const [k, v] of Object.entries(vars)) {
      text = text.replace(new RegExp(`\\{${k}\\}`, "g"), String(v));
    }
  }
  return applyBrand(text);
}

export const t = translate;
