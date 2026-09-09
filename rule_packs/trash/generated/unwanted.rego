package scryer.user.trash_guides_unwanted
import rego.v1


# Generated from parser snapshot 31a2716d03a3f554a5a2a6bd76456109d900af05.
trash_detection_tables := {
  "blocked_title_rules_by_anchor": {
    "1XBET": [
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "movie",
        "order": 40,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "1XBET"
          ]
        }
      }
    ],
    "2160P": [
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "series",
        "order": 50,
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "2160P",
            "BITOR"
          ]
        }
      }
    ],
    "ASUKA": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 0,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "ASUKA",
            "RAWS"
          ]
        }
      }
    ],
    "ASUKARAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 1,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "ASUKARAWS"
          ]
        }
      }
    ],
    "BEATRICE": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 2,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "BEATRICE",
            "RAWS"
          ]
        }
      }
    ],
    "BEATRICERAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 3,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "BEATRICERAWS"
          ]
        }
      }
    ],
    "BEN": [
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "movie",
        "order": 41,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "BEN",
            "THE",
            "MEN"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "series",
        "order": 51,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "BEN",
            "THE",
            "MEN"
          ]
        }
      }
    ],
    "CREATIVE24": [
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "series",
        "order": 52,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "CREATIVE24"
          ]
        }
      }
    ],
    "DADDY": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 4,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "DADDY",
            "RAWS"
          ]
        }
      }
    ],
    "DADDYRAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 5,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "DADDYRAWS"
          ]
        }
      }
    ],
    "FANSUB": [
      {
        "category": "anime",
        "code": "trash_guides_fansub",
        "facet": "anime",
        "order": 38,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "FANSUB"
          ]
        }
      }
    ],
    "FASTSUB": [
      {
        "category": "anime",
        "code": "trash_guides_fastsub",
        "facet": "anime",
        "order": 39,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "FASTSUB"
          ]
        }
      }
    ],
    "FERANKI1980": [
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "movie",
        "order": 42,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "FERANKI1980"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "series",
        "order": 53,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "FERANKI1980"
          ]
        }
      }
    ],
    "FUMI": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 6,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "FUMI",
            "RAWS"
          ]
        }
      }
    ],
    "FUMIRAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 7,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "FUMIRAWS"
          ]
        }
      }
    ],
    "GALAXYRG": [
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "movie",
        "order": 43,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "GALAXYRG"
          ]
        }
      }
    ],
    "IRIZA": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 8,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "IRIZA",
            "RAWS"
          ]
        }
      }
    ],
    "IRIZARAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 9,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "IRIZARAWS"
          ]
        }
      }
    ],
    "KAWAIIKA": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 10,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "KAWAIIKA",
            "RAWS"
          ]
        }
      }
    ],
    "KAWAIIKARAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 11,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "KAWAIIKARAWS"
          ]
        }
      }
    ],
    "KOI": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 12,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "KOI",
            "RAWS"
          ]
        }
      }
    ],
    "KOIRAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 13,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "KOIRAWS"
          ]
        }
      }
    ],
    "LILITH": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 14,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "LILITH",
            "RAWS"
          ]
        }
      }
    ],
    "LILITHRAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 15,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "LILITHRAWS"
          ]
        }
      }
    ],
    "LOWPOWER": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 16,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "LOWPOWER",
            "RAWS"
          ]
        }
      }
    ],
    "LOWPOWERRAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 17,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "LOWPOWERRAWS"
          ]
        }
      }
    ],
    "MOOZZI2": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 18,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "MOOZZI2"
          ]
        }
      }
    ],
    "NANAKO": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 19,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "NANAKO",
            "RAWS"
          ]
        }
      }
    ],
    "NANAKORAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 20,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "NANAKORAWS"
          ]
        }
      }
    ],
    "NC": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 21,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "NC",
            "RAWS"
          ]
        }
      }
    ],
    "NCRAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 22,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "NCRAWS"
          ]
        }
      }
    ],
    "NEKO": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 23,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "NEKO",
            "RAWS"
          ]
        }
      }
    ],
    "NEKORAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 24,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "NEKORAWS"
          ]
        }
      }
    ],
    "NEW": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 25,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "NEW",
            "RAWS"
          ]
        }
      }
    ],
    "NEWRAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 26,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "NEWRAWS"
          ]
        }
      }
    ],
    "OHYS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 27,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "OHYS",
            "RAWS"
          ]
        }
      }
    ],
    "OHYSRAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 28,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "OHYSRAWS"
          ]
        }
      }
    ],
    "PANDORATV": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 29,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PANDORATV",
            "RAWS"
          ]
        }
      }
    ],
    "PANDORATVRAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 30,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PANDORATVRAWS"
          ]
        }
      }
    ],
    "REINFORCE": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 31,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REINFORCE"
          ]
        }
      }
    ],
    "SCRYOUS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 32,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "SCRYOUS",
            "RAWS"
          ]
        }
      }
    ],
    "SCRYOUSRAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 33,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "SCRYOUSRAWS"
          ]
        }
      }
    ],
    "SEICHER": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 34,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "SEICHER",
            "RAWS"
          ]
        }
      }
    ],
    "SEICHERRAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 35,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "SEICHERRAWS"
          ]
        }
      }
    ],
    "SHINIORI": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 36,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "SHINIORI",
            "RAWS"
          ]
        }
      }
    ],
    "SHINIORIRAWS": [
      {
        "category": "anime",
        "code": "trash_guides_anime_raws",
        "facet": "anime",
        "order": 37,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "SHINIORIRAWS"
          ]
        }
      }
    ],
    "SWTYBLZ": [
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "movie",
        "order": 44,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "SWTYBLZ"
          ]
        }
      }
    ],
    "TEEWEE": [
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "movie",
        "order": 45,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "TEEWEE"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "series",
        "order": 54,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "TEEWEE"
          ]
        }
      }
    ],
    "TEKNO3D": [
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "movie",
        "order": 46,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "TEKNO3D"
          ]
        }
      }
    ],
    "WEB": [
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "movie",
        "order": 47,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "WEB",
            "DL",
            "RIP",
            "EVO"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "movie",
        "order": 48,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "WEB",
            "DL",
            "RIP",
            "PIRATES"
          ]
        }
      }
    ],
    "WILL1869": [
      {
        "category": "any",
        "code": "trash_guides_lq_release_title",
        "facet": "movie",
        "order": 49,
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "WILL1869"
          ]
        }
      }
    ]
  },
  "fact_rules_by_anchor": {
    "12": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "12",
            "D3",
            "AI",
            "ENHANCED",
            "UPS",
            "UHD",
            "UPSCALED",
            "UHD",
            "UPREZ"
          ]
        }
      }
    ],
    "12D3": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "12D3",
            "HEVC",
            "AI"
          ]
        }
      }
    ],
    "AI": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "AI",
            "ENHANCED"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "series",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "AI",
            "ENHANCED"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "series",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "D34P",
            "HEVC",
            "AI"
          ]
        }
      }
    ],
    "AIUS": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "AIUS"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "AIUS"
          ]
        }
      }
    ],
    "D34P": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "D34P",
            "AI",
            "ENHANCED",
            "UPS",
            "UHD",
            "UPSCALED",
            "UHD",
            "UPREZ"
          ]
        }
      }
    ],
    "DUB": [
      {
        "category": "any",
        "code": "trash.dubs_only",
        "facet": "anime",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "ENG",
            "DUB"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.dubs_only",
        "facet": "anime",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "FUNI",
            "DUB"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.dubs_only",
        "facet": "anime",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "DUB"
          ]
        }
      }
    ],
    "DUBBED": [
      {
        "category": "any",
        "code": "trash.dubs_only",
        "facet": "anime",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "DUBBED"
          ]
        }
      }
    ],
    "EZTVX": [
      {
        "category": "any",
        "code": "trash.retagged",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "EZTVX",
            "IO",
            "RE",
            "TO"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.retagged",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "EZTVX",
            "IO",
            "RE",
            "TO"
          ]
        }
      }
    ],
    "FANSUB": [
      {
        "category": "anime",
        "code": "trash.fansub",
        "facet": "anime",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "FANSUB"
          ]
        }
      },
      {
        "category": "anime",
        "code": "trash.hardcoded_subs",
        "facet": "anime",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "FANSUB"
          ]
        }
      }
    ],
    "FASTSUB": [
      {
        "category": "anime",
        "code": "trash.fastsub",
        "facet": "anime",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "FASTSUB"
          ]
        }
      },
      {
        "category": "anime",
        "code": "trash.hardcoded_subs",
        "facet": "anime",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "FASTSUB"
          ]
        }
      }
    ],
    "GUYZO": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "GUYZO"
          ]
        }
      }
    ],
    "ORARBG": [
      {
        "category": "any",
        "code": "trash.retagged",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "ORARBG"
          ]
        }
      }
    ],
    "PROPER": [
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "movie",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "REAL",
            "PROPER"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PROPER"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "series",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "REAL",
            "PROPER"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PROPER"
          ]
        }
      }
    ],
    "PROPER2": [
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PROPER2"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PROPER2"
          ]
        }
      }
    ],
    "PROPER3": [
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PROPER3"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PROPER3"
          ]
        }
      }
    ],
    "REAL": [
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "movie",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "REAL",
            "REPACK"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "series",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "REAL",
            "REPACK"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.repack",
        "facet": "movie",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "REAL",
            "REPACK"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.repack",
        "facet": "series",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "REAL",
            "REPACK"
          ]
        }
      }
    ],
    "REGRADE": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REGRADE"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REGRADE"
          ]
        }
      }
    ],
    "REGRADED": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REGRADED"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REGRADED"
          ]
        }
      }
    ],
    "REPACK": [
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.repack",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.repack",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK"
          ]
        }
      }
    ],
    "REPACK2": [
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK2"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK2"
          ]
        }
      }
    ],
    "REPACK3": [
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK3"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK3"
          ]
        }
      }
    ],
    "RERIP": [
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "RERIP"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.proper",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "RERIP"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.repack",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "RERIP"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.repack",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "RERIP"
          ]
        }
      }
    ],
    "RW": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "RW"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "RW"
          ]
        }
      }
    ],
    "THE": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "THE",
            "UPSCALER"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "THE",
            "UPSCALER"
          ]
        }
      }
    ],
    "THEUPSCALER": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "THEUPSCALER"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "THEUPSCALER"
          ]
        }
      }
    ],
    "UPREZ": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPREZ"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPREZ"
          ]
        }
      }
    ],
    "UPSCALED": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPSCALED"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPSCALED"
          ]
        }
      }
    ],
    "UPSCALEREGRADE": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPSCALEREGRADE"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPSCALEREGRADE"
          ]
        }
      }
    ],
    "UPSCALEREGRADED": [
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "movie",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPSCALEREGRADED"
          ]
        }
      },
      {
        "category": "any",
        "code": "trash.ai_enhanced",
        "facet": "series",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPSCALEREGRADED"
          ]
        }
      }
    ]
  },
  "locale_group_exact": {},
  "locale_group_prefix": {},
  "no_release_group_fact_facets": [
    "movie",
    "series"
  ],
  "service_aliases_by_token": {
    "4OD": [
      {
        "requires_web_adjacency": false,
        "service": "Channel 4"
      }
    ],
    "ABEMA": [
      {
        "requires_web_adjacency": false,
        "service": "ABEMA"
      }
    ],
    "ABEMATV": [
      {
        "requires_web_adjacency": false,
        "service": "ABEMA"
      }
    ],
    "ADN": [
      {
        "requires_web_adjacency": false,
        "service": "ADN"
      }
    ],
    "ALL4": [
      {
        "requires_web_adjacency": false,
        "service": "Channel 4"
      }
    ],
    "AMAZON": [
      {
        "requires_web_adjacency": false,
        "service": "Amazon"
      }
    ],
    "AMAZONHD": [
      {
        "requires_web_adjacency": false,
        "service": "Amazon"
      }
    ],
    "AMZN": [
      {
        "requires_web_adjacency": false,
        "service": "Amazon"
      }
    ],
    "APPLE": [
      {
        "requires_web_adjacency": false,
        "service": "Apple TV+"
      }
    ],
    "APTV": [
      {
        "requires_web_adjacency": false,
        "service": "Apple TV+"
      }
    ],
    "ATV": [
      {
        "requires_web_adjacency": false,
        "service": "ATV"
      }
    ],
    "ATVP": [
      {
        "requires_web_adjacency": false,
        "service": "Apple TV+"
      }
    ],
    "AUBC": [
      {
        "requires_web_adjacency": false,
        "service": "ABC iview"
      }
    ],
    "BBC": [
      {
        "requires_web_adjacency": false,
        "service": "BBC iPlayer"
      }
    ],
    "BBCI": [
      {
        "requires_web_adjacency": false,
        "service": "BBC iPlayer"
      }
    ],
    "BCORE": [
      {
        "requires_web_adjacency": false,
        "service": "BCORE"
      }
    ],
    "BGLOBAL": [
      {
        "requires_web_adjacency": false,
        "service": "B-Global"
      }
    ],
    "BILI": [
      {
        "requires_web_adjacency": false,
        "service": "Bilibili"
      }
    ],
    "BILIBILI": [
      {
        "requires_web_adjacency": false,
        "service": "Bilibili"
      }
    ],
    "CBC": [
      {
        "requires_web_adjacency": false,
        "service": "CBC Gem"
      }
    ],
    "CNLP": [
      {
        "requires_web_adjacency": true,
        "service": "CANAL+"
      }
    ],
    "CPNG": [
      {
        "requires_web_adjacency": false,
        "service": "Coupang Play"
      }
    ],
    "CR": [
      {
        "requires_web_adjacency": false,
        "service": "Crunchyroll"
      }
    ],
    "CRAV": [
      {
        "requires_web_adjacency": true,
        "service": "Crave"
      }
    ],
    "CRAVE": [
      {
        "requires_web_adjacency": true,
        "service": "Crave"
      }
    ],
    "CROLL": [
      {
        "requires_web_adjacency": false,
        "service": "Crunchyroll"
      }
    ],
    "CRUNCHY": [
      {
        "requires_web_adjacency": false,
        "service": "Crunchyroll"
      }
    ],
    "CRUNCHYR": [
      {
        "requires_web_adjacency": false,
        "service": "Crunchyroll"
      }
    ],
    "CRUNCHYROLL": [
      {
        "requires_web_adjacency": false,
        "service": "Crunchyroll"
      }
    ],
    "DCU": [
      {
        "requires_web_adjacency": false,
        "service": "DC Universe"
      }
    ],
    "DISNEY": [
      {
        "requires_web_adjacency": false,
        "service": "Disney+"
      }
    ],
    "DMM": [
      {
        "requires_web_adjacency": false,
        "service": "DMM TV"
      }
    ],
    "DNSP": [
      {
        "requires_web_adjacency": false,
        "service": "Disney+"
      }
    ],
    "DSCP": [
      {
        "requires_web_adjacency": true,
        "service": "Discovery+"
      }
    ],
    "DSCV": [
      {
        "requires_web_adjacency": true,
        "service": "Discovery+"
      }
    ],
    "DSNP": [
      {
        "requires_web_adjacency": false,
        "service": "Disney+"
      }
    ],
    "DSNPHS": [
      {
        "requires_web_adjacency": false,
        "service": "Disney+ Hotstar"
      }
    ],
    "DSNY": [
      {
        "requires_web_adjacency": false,
        "service": "Disney+"
      }
    ],
    "FOD": [
      {
        "requires_web_adjacency": false,
        "service": "FOD"
      }
    ],
    "FRIDAY": [
      {
        "requires_web_adjacency": true,
        "service": "friDay Video"
      }
    ],
    "FUNI": [
      {
        "requires_web_adjacency": false,
        "service": "Funimation"
      }
    ],
    "FUNIMATION": [
      {
        "requires_web_adjacency": false,
        "service": "Funimation"
      }
    ],
    "HAMI": [
      {
        "requires_web_adjacency": false,
        "service": "Hami Video"
      }
    ],
    "HAMIVIDEO": [
      {
        "requires_web_adjacency": false,
        "service": "Hami Video"
      }
    ],
    "HBO": [
      {
        "requires_web_adjacency": false,
        "service": "HBO Max"
      }
    ],
    "HBOM": [
      {
        "requires_web_adjacency": false,
        "service": "HBO Max"
      }
    ],
    "HBOMAX": [
      {
        "requires_web_adjacency": false,
        "service": "HBO Max"
      }
    ],
    "HIDI": [
      {
        "requires_web_adjacency": false,
        "service": "HIDIVE"
      }
    ],
    "HIDIVE": [
      {
        "requires_web_adjacency": false,
        "service": "HIDIVE"
      }
    ],
    "HMAX": [
      {
        "requires_web_adjacency": false,
        "service": "HBO Max"
      }
    ],
    "HOTSTAR": [
      {
        "requires_web_adjacency": false,
        "service": "Hotstar"
      }
    ],
    "HTSR": [
      {
        "requires_web_adjacency": false,
        "service": "Disney+ Hotstar"
      }
    ],
    "HULU": [
      {
        "requires_web_adjacency": false,
        "service": "Hulu"
      }
    ],
    "IP": [
      {
        "requires_web_adjacency": true,
        "service": "BBC iPlayer"
      }
    ],
    "IPLAYER": [
      {
        "requires_web_adjacency": false,
        "service": "BBC iPlayer"
      }
    ],
    "IQIY": [
      {
        "requires_web_adjacency": true,
        "service": "iQIYI"
      }
    ],
    "IQIYI": [
      {
        "requires_web_adjacency": true,
        "service": "iQIYI"
      }
    ],
    "IT": [
      {
        "requires_web_adjacency": true,
        "service": "iTunes"
      }
    ],
    "ITUNES": [
      {
        "requires_web_adjacency": false,
        "service": "iTunes"
      }
    ],
    "ITV": [
      {
        "requires_web_adjacency": true,
        "service": "ITVX"
      }
    ],
    "ITVX": [
      {
        "requires_web_adjacency": true,
        "service": "ITVX"
      }
    ],
    "KCW": [
      {
        "requires_web_adjacency": false,
        "service": "KOCOWA"
      }
    ],
    "KKTV": [
      {
        "requires_web_adjacency": false,
        "service": "KKTV"
      }
    ],
    "KOCOWA": [
      {
        "requires_web_adjacency": false,
        "service": "KOCOWA"
      }
    ],
    "LINETV": [
      {
        "requires_web_adjacency": false,
        "service": "LINE TV"
      }
    ],
    "MAX": [
      {
        "requires_web_adjacency": false,
        "service": "HBO Max"
      }
    ],
    "MY5": [
      {
        "requires_web_adjacency": false,
        "service": "My5"
      }
    ],
    "MYTVSUPER": [
      {
        "requires_web_adjacency": false,
        "service": "myTV SUPER"
      }
    ],
    "NETFLIX": [
      {
        "requires_web_adjacency": false,
        "service": "Netflix"
      }
    ],
    "NETFLIXHD": [
      {
        "requires_web_adjacency": false,
        "service": "Netflix"
      }
    ],
    "NETFLIXUHD": [
      {
        "requires_web_adjacency": false,
        "service": "Netflix"
      }
    ],
    "NF": [
      {
        "requires_web_adjacency": false,
        "service": "Netflix"
      }
    ],
    "NLZ": [
      {
        "requires_web_adjacency": false,
        "service": "NLZiet"
      }
    ],
    "NLZIET": [
      {
        "requires_web_adjacency": false,
        "service": "NLZiet"
      }
    ],
    "NOW": [
      {
        "requires_web_adjacency": true,
        "service": "NOW"
      }
    ],
    "OVID": [
      {
        "requires_web_adjacency": false,
        "service": "OVID.tv"
      }
    ],
    "PARAMOUNT": [
      {
        "requires_web_adjacency": false,
        "service": "Paramount+"
      }
    ],
    "PATHE": [
      {
        "requires_web_adjacency": false,
        "service": "Pathé Thuis"
      }
    ],
    "PCOK": [
      {
        "requires_web_adjacency": false,
        "service": "Peacock"
      }
    ],
    "PEACOCK": [
      {
        "requires_web_adjacency": false,
        "service": "Peacock"
      }
    ],
    "PLAY": [
      {
        "requires_web_adjacency": true,
        "service": "PLAY"
      }
    ],
    "PMTP": [
      {
        "requires_web_adjacency": false,
        "service": "Paramount+"
      }
    ],
    "QIBI": [
      {
        "requires_web_adjacency": false,
        "service": "Quibi"
      }
    ],
    "QUIBI": [
      {
        "requires_web_adjacency": false,
        "service": "Quibi"
      }
    ],
    "RED": [
      {
        "requires_web_adjacency": true,
        "service": "YouTube Premium"
      }
    ],
    "ROKU": [
      {
        "requires_web_adjacency": false,
        "service": "The Roku Channel"
      }
    ],
    "SALTO": [
      {
        "requires_web_adjacency": false,
        "service": "Salto"
      }
    ],
    "SHO": [
      {
        "requires_web_adjacency": true,
        "service": "Showtime"
      }
    ],
    "SHOWTIME": [
      {
        "requires_web_adjacency": false,
        "service": "Showtime"
      }
    ],
    "STAN": [
      {
        "requires_web_adjacency": false,
        "service": "Stan"
      }
    ],
    "STRP": [
      {
        "requires_web_adjacency": false,
        "service": "Star+"
      }
    ],
    "SYFY": [
      {
        "requires_web_adjacency": false,
        "service": "SYFY"
      }
    ],
    "TVER": [
      {
        "requires_web_adjacency": false,
        "service": "TVer"
      }
    ],
    "TVING": [
      {
        "requires_web_adjacency": true,
        "service": "TVING"
      }
    ],
    "VDL": [
      {
        "requires_web_adjacency": false,
        "service": "Videoland"
      }
    ],
    "VIDEOLAND": [
      {
        "requires_web_adjacency": false,
        "service": "Videoland"
      }
    ],
    "VIKI": [
      {
        "requires_web_adjacency": false,
        "service": "Viki"
      }
    ],
    "VIU": [
      {
        "requires_web_adjacency": true,
        "service": "Viu"
      }
    ],
    "VRV": [
      {
        "requires_web_adjacency": false,
        "service": "VRV"
      }
    ],
    "WAKA": [
      {
        "requires_web_adjacency": false,
        "service": "Wakanim"
      }
    ],
    "WAKANIM": [
      {
        "requires_web_adjacency": false,
        "service": "Wakanim"
      }
    ],
    "WAVVE": [
      {
        "requires_web_adjacency": false,
        "service": "Wavve"
      }
    ],
    "WETV": [
      {
        "requires_web_adjacency": false,
        "service": "WeTV"
      }
    ],
    "WKN": [
      {
        "requires_web_adjacency": false,
        "service": "Wakanim"
      }
    ],
    "YOUKU": [
      {
        "requires_web_adjacency": false,
        "service": "Youku"
      }
    ],
    "YOUTUBE": [
      {
        "requires_web_adjacency": false,
        "service": "YouTube"
      }
    ]
  },
  "token_signal_rules_by_anchor": {
    "12": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "12",
            "D3",
            "AI",
            "ENHANCED",
            "UPS",
            "UHD",
            "UPSCALED",
            "UHD",
            "UPREZ"
          ]
        }
      }
    ],
    "12D3": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "12D3",
            "HEVC",
            "AI"
          ]
        }
      }
    ],
    "AI": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "AI",
            "ENHANCED"
          ]
        }
      },
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "AI",
            "ENHANCED"
          ]
        }
      },
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "D34P",
            "HEVC",
            "AI"
          ]
        }
      }
    ],
    "AIUS": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "AIUS"
          ]
        }
      },
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "AIUS"
          ]
        }
      }
    ],
    "D34P": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "D34P",
            "AI",
            "ENHANCED",
            "UPS",
            "UHD",
            "UPSCALED",
            "UHD",
            "UPREZ"
          ]
        }
      }
    ],
    "DUB": [
      {
        "kind": "dubs_only",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "ENG",
            "DUB"
          ]
        }
      },
      {
        "kind": "dubs_only",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "FUNI",
            "DUB"
          ]
        }
      },
      {
        "kind": "dubs_only",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "DUB"
          ]
        }
      }
    ],
    "DUBBED": [
      {
        "kind": "dubs_only",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "DUBBED"
          ]
        }
      }
    ],
    "GUYZO": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "GUYZO"
          ]
        }
      }
    ],
    "PROPER": [
      {
        "kind": "proper",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "REAL",
            "PROPER"
          ]
        }
      },
      {
        "kind": "proper",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PROPER"
          ]
        }
      },
      {
        "kind": "proper",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "REAL",
            "PROPER"
          ]
        }
      },
      {
        "kind": "proper",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PROPER"
          ]
        }
      }
    ],
    "PROPER2": [
      {
        "kind": "proper",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PROPER2"
          ]
        }
      },
      {
        "kind": "proper",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PROPER2"
          ]
        }
      }
    ],
    "PROPER3": [
      {
        "kind": "proper",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PROPER3"
          ]
        }
      },
      {
        "kind": "proper",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "PROPER3"
          ]
        }
      }
    ],
    "REAL": [
      {
        "kind": "repack",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "REAL",
            "REPACK"
          ]
        }
      },
      {
        "kind": "repack",
        "pattern": {
          "kind": "required_tokens",
          "tokens": [
            "REAL",
            "REPACK"
          ]
        }
      }
    ],
    "REGRADE": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REGRADE"
          ]
        }
      },
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REGRADE"
          ]
        }
      }
    ],
    "REGRADED": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REGRADED"
          ]
        }
      },
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REGRADED"
          ]
        }
      }
    ],
    "REPACK": [
      {
        "kind": "repack",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK"
          ]
        }
      },
      {
        "kind": "repack",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK"
          ]
        }
      }
    ],
    "REPACK2": [
      {
        "kind": "proper",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK2"
          ]
        }
      },
      {
        "kind": "proper",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK2"
          ]
        }
      }
    ],
    "REPACK3": [
      {
        "kind": "proper",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK3"
          ]
        }
      },
      {
        "kind": "proper",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "REPACK3"
          ]
        }
      }
    ],
    "RERIP": [
      {
        "kind": "repack",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "RERIP"
          ]
        }
      },
      {
        "kind": "repack",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "RERIP"
          ]
        }
      }
    ],
    "RW": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "RW"
          ]
        }
      },
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "RW"
          ]
        }
      }
    ],
    "THE": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "THE",
            "UPSCALER"
          ]
        }
      },
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "THE",
            "UPSCALER"
          ]
        }
      }
    ],
    "THEUPSCALER": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "THEUPSCALER"
          ]
        }
      },
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "THEUPSCALER"
          ]
        }
      }
    ],
    "UPREZ": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPREZ"
          ]
        }
      },
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPREZ"
          ]
        }
      }
    ],
    "UPSCALED": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPSCALED"
          ]
        }
      },
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPSCALED"
          ]
        }
      }
    ],
    "UPSCALEREGRADE": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPSCALEREGRADE"
          ]
        }
      },
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPSCALEREGRADE"
          ]
        }
      }
    ],
    "UPSCALEREGRADED": [
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPSCALEREGRADED"
          ]
        }
      },
      {
        "kind": "ai_enhanced",
        "pattern": {
          "kind": "sequence",
          "tokens": [
            "UPSCALEREGRADED"
          ]
        }
      }
    ]
  }
}

# No raw-title fallback exists. Token positions and Unicode normalization belong to the parser.
normalized_tokens := input.release.normalized_tokens if {
  is_array(input.release.normalized_tokens)
}

detection_available if { normalized_tokens }

# Rust uses to_ascii_lowercase / eq_ignore_ascii_case for these comparisons.
# Keep non-ASCII lookalikes distinct from ASCII release-group spellings.
trash_detection_ascii_fold(value) := result if {
  a := replace(value, "A", "a"); b := replace(a, "B", "b"); c := replace(b, "C", "c"); d := replace(c, "D", "d"); e := replace(d, "E", "e"); f := replace(e, "F", "f"); g := replace(f, "G", "g"); h := replace(g, "H", "h"); i := replace(h, "I", "i"); j := replace(i, "J", "j"); k := replace(j, "K", "k"); l := replace(k, "L", "l"); m := replace(l, "M", "m"); n := replace(m, "N", "n"); o := replace(n, "O", "o"); p := replace(o, "P", "p"); q := replace(p, "Q", "q"); r := replace(q, "R", "r"); s := replace(r, "S", "s"); t := replace(s, "T", "t"); u := replace(t, "U", "u"); v := replace(u, "V", "v"); w := replace(v, "W", "w"); x := replace(w, "X", "x"); y := replace(x, "Y", "y"); result := replace(y, "Z", "z")
}
trash_detection_ascii_upper(value) := result if {
  a := replace(value, "a", "A"); b := replace(a, "b", "B"); c := replace(b, "c", "C"); d := replace(c, "d", "D"); e := replace(d, "e", "E"); f := replace(e, "f", "F"); g := replace(f, "g", "G"); h := replace(g, "h", "H"); i := replace(h, "i", "I"); j := replace(i, "j", "J"); k := replace(j, "k", "K"); l := replace(k, "l", "L"); m := replace(l, "m", "M"); n := replace(m, "n", "N"); o := replace(n, "o", "O"); p := replace(o, "p", "P"); q := replace(p, "q", "Q"); r := replace(q, "r", "R"); s := replace(r, "s", "S"); t := replace(s, "t", "T"); u := replace(t, "u", "U"); v := replace(u, "v", "V"); w := replace(v, "w", "W"); x := replace(w, "x", "X"); y := replace(x, "y", "Y"); result := replace(y, "z", "Z")
}

category_value := value if { value := object.get(input.context, "category", ""); is_string(value) }
category_value := "" if { not is_string(object.get(input.context, "category", "")) }
detection_facet := "anime" if { trash_detection_ascii_fold(trim_space(category_value)) == "anime" }
detection_facet := "series" if { trash_detection_ascii_fold(trim_space(category_value)) == "series" }
detection_facet := "movie" if { not trash_detection_ascii_fold(trim_space(category_value)) == "anime"; not trash_detection_ascii_fold(trim_space(category_value)) == "series" }
detection_scope := "anime" if { detection_facet == "anime" }
detection_scope := "any" if { not detection_facet == "anime" }

pattern_matches(pattern, tokens) if {
  pattern.kind == "required_tokens"
  every wanted in pattern.tokens { some candidate in tokens; candidate == wanted }
}
pattern_matches(pattern, tokens) if {
  pattern.kind == "sequence"
  count(pattern.tokens) > 0
  some start
  tokens[start] == pattern.tokens[0]
  start + count(pattern.tokens) <= count(tokens)
  every offset, wanted in pattern.tokens { tokens[start + offset] == wanted }
}

rule_applies(rule) if { rule.facet == detection_facet; rule.category == "any" }
rule_applies(rule) if { rule.facet == detection_facet; rule.category == "anime"; detection_scope == "anime" }

signal_codes(rule) := ["trash.ai_enhanced"] if { rule.kind == "ai_enhanced" }
signal_codes(rule) := ["trash.proper"] if { rule.kind == "proper" }
signal_codes(rule) := ["trash.proper", "trash.repack"] if { rule.kind == "repack" }
signal_codes(rule) := ["trash.dubs_only"] if { rule.kind == "dubs_only" }
signal_codes(rule) := ["trash.hardcoded_subs"] if { rule.kind == "hardcoded_subs" }

blocked_fact(rule) := "trash.blocked.anime_raws" if { rule.code == "trash_guides_anime_raws" }
blocked_fact(rule) := "trash.blocked.lq_release_title" if { rule.code == "trash_guides_lq_release_title" }
blocked_fact(rule) := "trash.blocked.fansub" if { rule.code == "trash_guides_fansub" }
blocked_fact(rule) := "trash.blocked.fastsub" if { rule.code == "trash_guides_fastsub" }
blocked_fact(rule) := "trash.blocked.legacy" if { not rule.code == "trash_guides_anime_raws"; not rule.code == "trash_guides_lq_release_title"; not rule.code == "trash_guides_fansub"; not rule.code == "trash_guides_fastsub" }

french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VF2($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFF[.]VFF($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFF[.]VFQ($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFQ[.]VFF($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFQ[.]VFQ($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFF VFF($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFF VFQ($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFQ VFF($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }
french_vf2_exclusion if { regex.match("(^|[^A-Z0-9])VFQ VFQ($|[^A-Z0-9])", trash_detection_ascii_upper(object.get(input.release, "raw_title", ""))) }

german_invalid_gap(language, subtitle) if {
  some gap_index
  gap_index > language
  gap_index < subtitle
  gap := normalized_tokens[gap_index]
  not regex.match("^[A-Z]+$", gap)
}
german_invalid_gap(language, subtitle) if {
  some gap_index
  gap_index > language
  gap_index < subtitle
  contains(normalized_tokens[gap_index], "DUB")
}
german_invalid_gap(language, subtitle) if {
  some gap_index
  gap_index > language
  gap_index < subtitle
  normalized_tokens[gap_index] in {"DL", "ML"}
}
german_subbed if {
  some language
  normalized_tokens[language] in {"GER", "GERMAN"}
  some subtitle
  subtitle > language
  normalized_tokens[subtitle] in {"OMU", "SUB", "SUBBED", "SUBS"}
  not german_invalid_gap(language, subtitle)
}

locale_context_matches(context) if { context == "any" }
locale_context_matches(context) if { context == "anime"; detection_facet == "anime" }
locale_context_matches(context) if { context == "anime_bd"; detection_facet == "anime"; trash_detection_ascii_fold(release_source) in {"bluray", "br-disk", "brdisk"} }
locale_context_matches(context) if { context == "anime_web"; detection_facet == "anime"; trash_detection_ascii_fold(release_source) in {"web-dl", "webrip"} }
release_source := value if { value := object.get(input.release, "source", ""); is_string(value) }
release_group_value := value if { value := object.get(input.release, "release_group", ""); is_string(value) }
release_quality := value if { value := object.get(input.release, "quality", ""); is_string(value) }
release_group_folded := trash_detection_ascii_fold(release_group_value) if { release_group_value }
locale_context_matches(context) if { context == "web"; trash_detection_ascii_fold(release_source) in {"web-dl", "webrip"} }
locale_context_matches(context) if { context == "remux"; input.release.is_remux }
locale_context_matches(context) if { context == "bluray"; trash_detection_ascii_fold(release_source) == "bluray"; not input.release.is_remux; not contains(release_quality, "2160") }
locale_context_matches(context) if { context == "uhd_bluray"; trash_detection_ascii_fold(release_source) == "bluray"; not input.release.is_remux; contains(release_quality, "2160") }
detected_facts[code] if {
  normalized_tokens
  some token in normalized_tokens
  some rule in trash_detection_tables.token_signal_rules_by_anchor[token]
  pattern_matches(rule.pattern, normalized_tokens)
  some code in signal_codes(rule)
}
detected_facts[code] if {
  normalized_tokens
  some token in normalized_tokens
  some rule in trash_detection_tables.blocked_title_rules_by_anchor[token]
  rule_applies(rule)
  pattern_matches(rule.pattern, normalized_tokens)
  code := blocked_fact(rule)
}
detected_facts[code] if {
  normalized_tokens
  some token in normalized_tokens
  some rule in trash_detection_tables.fact_rules_by_anchor[token]
  rule_applies(rule)
  pattern_matches(rule.pattern, normalized_tokens)
  code := rule.code
  code != "trash.locale.german.marker.subbed"
  not french_vf2_exclusion
}
detected_facts[code] if {
  normalized_tokens
  some token in normalized_tokens
  some rule in trash_detection_tables.fact_rules_by_anchor[token]
  rule_applies(rule)
  pattern_matches(rule.pattern, normalized_tokens)
  code := rule.code
  code != "trash.locale.french.marker.vff"
  code != "trash.locale.french.marker.vfq"
  code != "trash.locale.german.marker.subbed"
  french_vf2_exclusion
}
detected_facts["trash.locale.german.marker.subbed"] if {
  normalized_tokens
  some token in normalized_tokens
  some rule in trash_detection_tables.fact_rules_by_anchor[token]
  rule_applies(rule)
  pattern_matches(rule.pattern, normalized_tokens)
  rule.code == "trash.locale.german.marker.subbed"
  german_subbed
}
detected_facts[code] if {
  normalized_tokens
  group := release_group_value
  group != ""
  some rule in trash_detection_tables.locale_group_exact[concat("|", [detection_facet, release_group_folded])]
  locale_context_matches(rule[1])
  code := rule[0]
}
detected_facts[code] if {
  normalized_tokens
  group := release_group_value
  group != ""
  some rule in trash_detection_tables.locale_group_prefix[detection_facet]
  startswith(release_group_folded, trash_detection_ascii_fold(rule[0]))
  locale_context_matches(rule[2])
  code := rule[1]
}
detected_facts["trash.no_release_group"] if {
  normalized_tokens
  object.get(input.release, "release_group", null) in {null, ""}
  some facet in trash_detection_tables.no_release_group_fact_facets
  facet == detection_facet
}

has_detected_fact(code) if { detected_facts[code] }
detected_fact_bool(code) := true if { detected_facts[code] }
detected_fact_bool(code) := false if { not detected_facts[code] }

# This diagnostic mirrors Rust detect_token_signals, rather than all derived
# facts: blocked/fact rules may independently project hardcoded-subs.
detected_token_signal(kind) if {
  normalized_tokens
  some token in normalized_tokens
  some rule in trash_detection_tables.token_signal_rules_by_anchor[token]
  pattern_matches(rule.pattern, normalized_tokens)
  rule.kind == kind
}
detected_token_signal_bool(kind) := true if { detected_token_signal(kind) }
detected_token_signal_bool(kind) := false if { not detected_token_signal(kind) }
detected_proper_signal if { detected_token_signal("proper") }
detected_proper_signal if { detected_token_signal("repack") }
detected_proper_signal_bool := true if { detected_proper_signal }
detected_proper_signal_bool := false if { not detected_proper_signal }

detected_services[service] if {
  normalized_tokens
  some index
  token := normalized_tokens[index]
  some rule in trash_detection_tables.service_aliases_by_token[token]
  not rule.requires_web_adjacency
  service := rule.service
}
detected_services[service] if {
  normalized_tokens
  some index
  token := normalized_tokens[index]
  some rule in trash_detection_tables.service_aliases_by_token[token]
  rule.requires_web_adjacency
  next := normalized_tokens[index + 1]
  next in {"WEB", "WEBDL", "WEBRIP"}
  service := rule.service
}

# The parser assigns StreamingService roles before selecting this scalar. Alias
# candidates above are diagnostic only; scoring must use this parser-selected
# input when it needs the current first-role precedence.
selected_service := service if { service := object.get(input.release, "streaming_service", ""); is_string(service); service != "" }

detected_signals := {"ai_enhanced": detected_token_signal_bool("ai_enhanced"), "proper": detected_proper_signal_bool, "repack": detected_token_signal_bool("repack"), "dubs_only": detected_token_signal_bool("dubs_only"), "hardcoded_subs": detected_token_signal_bool("hardcoded_subs")}
detected_blocked_title := code if {
  normalized_tokens
  matching_orders := [rule.order | some token in normalized_tokens; some rule in trash_detection_tables.blocked_title_rules_by_anchor[token]; rule_applies(rule); pattern_matches(rule.pattern, normalized_tokens)]
  first_order := min(matching_orders)
  some token in normalized_tokens
  some rule in trash_detection_tables.blocked_title_rules_by_anchor[token]
  rule.order == first_order
  rule_applies(rule)
  pattern_matches(rule.pattern, normalized_tokens)
  code := rule.code
}
detector_result := {"available": true, "facts": sort(object.keys(detected_facts)), "services": sort(object.keys(detected_services)), "selected_service": selected_service, "signals": detected_signals, "blocked": detected_blocked_title} if { detection_available; detected_blocked_title; selected_service }
detector_result := {"available": true, "facts": sort(object.keys(detected_facts)), "services": sort(object.keys(detected_services)), "selected_service": null, "signals": detected_signals, "blocked": detected_blocked_title} if { detection_available; detected_blocked_title; not selected_service }
detector_result := {"available": true, "facts": sort(object.keys(detected_facts)), "services": sort(object.keys(detected_services)), "selected_service": selected_service, "signals": detected_signals, "blocked": null} if { detection_available; not detected_blocked_title; selected_service }
detector_result := {"available": true, "facts": sort(object.keys(detected_facts)), "services": sort(object.keys(detected_services)), "selected_service": null, "signals": detected_signals, "blocked": null} if { detection_available; not detected_blocked_title; not selected_service }
detector_result := {"available": false, "reason": "release.normalized_tokens is required"} if { not detection_available }


trash_unwanted_weight(name) := value if { weights:={"balanced":{"scene":-30,"obfuscated":-90,"retagged":-60,"hardcoded":-300},"audiophile":{"scene":-60,"obfuscated":-180,"retagged":-120,"hardcoded":-400},"efficient":{"scene":-15,"obfuscated":-45,"retagged":-30,"hardcoded":-200},"compatible":{"scene":-20,"obfuscated":-60,"retagged":-40,"hardcoded":-300}}; value:=object.get(object.get(weights,lower(object.get(input.profile,"scoring_persona","balanced")),weights["balanced"]),name,0) }
score_entry["trash.scene"] := trash_unwanted_weight("scene") if { has_detected_fact("trash.scene") }
score_entry["trash.obfuscated"] := trash_unwanted_weight("obfuscated") if { has_detected_fact("trash.obfuscated") }
score_entry["trash.retagged"] := trash_unwanted_weight("retagged") if { has_detected_fact("trash.retagged") }
score_entry["hardcoded_subs"] := trash_unwanted_weight("hardcoded") if { input.release.is_hardcoded_subs == true }
trash_upscaled if { input.release.is_ai_enhanced == true }
trash_upscaled if { has_detected_fact("trash.ai_enhanced") }
score_entry["ai_enhanced_upscaled"] := -10000 if {
  trash_upscaled
  object.get(object.get(input.profile, "scoring_overrides", {}), "block_upscaled", null) != false
}
score_entry["trash.no_release_group"] := -10000 if { has_detected_fact("trash.no_release_group") }
score_entry["trash_guides_anime_raws"] := -10000 if { has_detected_fact("trash.blocked.anime_raws") }
score_entry["trash_guides_lq_release_title"] := -10000 if { has_detected_fact("trash.blocked.lq_release_title") }
score_entry["trash_guides_fansub"] := -10000 if { has_detected_fact("trash.blocked.fansub") }
score_entry["trash_guides_fastsub"] := -10000 if { has_detected_fact("trash.blocked.fastsub") }
