import { randomInt as nodeRandomInt } from "node:crypto";

const WORDS = [
  "amber", "anchor", "apricot", "atlas", "birch", "breeze", "brook", "cedar",
  "cirrus", "clover", "comet", "coral", "cosmos", "crane", "dawn", "delta",
  "ember", "fern", "finch", "fjord", "flint", "grove", "harbor", "hazel",
  "heron", "iris", "juniper", "kite", "lagoon", "lark", "linen", "lotus",
  "lumen", "maple", "marble", "meadow", "moss", "nova", "olive", "onyx",
  "orbit", "otter", "pebble", "pine", "plume", "quartz", "raven", "reef",
  "ridge", "river", "sable", "sage", "shell", "solstice", "sparrow", "spruce",
  "stone", "summit", "tide", "timber", "vale", "willow", "wren", "zephyr",
] as const;

type TaskIdLookup = (candidate: string) => boolean;

/** Start with two words; add a third only when the local task history collides. */
export function createTaskId(
  isTaken: TaskIdLookup = () => false,
  randomInt: (upperBound: number) => number = nodeRandomInt,
): string {
  const firstIndex = randomInt(WORDS.length);
  const secondChoice = randomInt(WORDS.length - 1);
  const secondIndex = secondChoice >= firstIndex ? secondChoice + 1 : secondChoice;
  const first = WORDS[firstIndex]!;
  const second = WORDS[secondIndex]!;
  const short = `${first}-${second}`;
  if (!isTaken(short)) return short;

  const thirdStart = randomInt(WORDS.length);
  for (let offset = 0; offset < WORDS.length; offset += 1) {
    const third = WORDS[(thirdStart + offset) % WORDS.length]!;
    if (third === first || third === second) continue;
    const expanded = `${short}-${third}`;
    if (!isTaken(expanded)) return expanded;
  }
  throw new Error(`Mnemonic task id space exhausted for ${short}`);
}
