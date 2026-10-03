// Source-native asset conversion. Supply Sharp through NODE_PATH; no app dependency.
const fs = require("fs");
const path = require("path");
const sharp = require("sharp");
const catalog = path.dirname(__dirname);
const names = ["AppIcon", "PikaMark", "ProviderOpenAI", "ProviderClaude", "ProviderOpenCode", "ProviderMuse"];
async function main() {
  await Promise.all(names.map(async name => {
    const directory = path.join(catalog, name + (name === "AppIcon" ? ".appiconset" : ".imageset"));
    let image = sharp(path.join(__dirname, name + ".svg"));
    if (name === "AppIcon") image = image.flatten({ background: "#29251f" }).removeAlpha();
    await image.png().toFile(path.join(directory, name + ".png"));
  }));
  const layers = await Promise.all(names.map(async (name, index) => ({
    input: await sharp(path.join(catalog, name + (name === "AppIcon" ? ".appiconset" : ".imageset"), name + ".png")).resize(200, 200, { fit: "contain" }).toBuffer(),
    left: 24 + index * 224, top: 24
  })));
  await sharp({ create: { width: 1368, height: 248, channels: 3, background: "#f6f7fb" } }).composite(layers).png().toFile(path.join(__dirname, "preview.png"));
}
main().catch(error => { console.error(error); process.exitCode = 1; });
