A photograph cropped down to a small piece is found by the faces in it. Two
small neural-network models, both distributed with the
[OpenCV Zoo](https://github.com/opencv/opencv_zoo), are carried in the
application's `models` folder and run on the computer by the Rust library
[tract](https://github.com/sonos/tract) (listed with the Rust libraries below);
nothing is sent anywhere.

* **YuNet**, which finds faces (`models/yunet.onnx`), by Shiqi Yu and
  contributors, under the MIT licence.
* **SFace**, which describes a face with 128 numbers (`models/sface.onnx`),
  under the Apache License, Version 2.0.

The models are used unmodified.
