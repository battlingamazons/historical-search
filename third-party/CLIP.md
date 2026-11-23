Picture search finds photographs by what they show, rather than by what the
catalog says about them. It uses OpenAI's **CLIP** (the ViT-B/16 model,
released by OpenAI under the MIT licence), which reads a picture or a phrase and
describes it with 512 numbers; pictures and phrases that mean the same thing get
similar numbers. It runs on the computer with the Rust library
[tract](https://github.com/sonos/tract) (listed with the Rust libraries below);
the pictures never leave it.

Two things from CLIP are involved:

* **The vocabulary and merge list** for turning a typed phrase into pieces of
  words (`src-tauri/clip/vocab.json` and `merges.txt`) are carried in the
  application, as published with the model.
* **The two model files** (about 300 MB together) are *not* carried in the
  installer. When the person turns picture search on and presses the download
  button, the program fetches them once from a fixed revision of the public
  Hugging Face repository
  [Xenova/clip-vit-base-patch16](https://huggingface.co/Xenova/clip-vit-base-patch16),
  which holds OpenAI's weights converted to the ONNX format with half-precision
  numbers, and checks each against a recorded SHA-256 checksum before using it.
  This is the only time the program uses the internet, and it sends nothing
  about the collection. The files are used unmodified.
