// 4 position + 56 common + (3 tail + 1 packed or 2 tail + 2 split) = 64 scalars.
struct Vertex {
    float4 pos : SV_Position;
    float4 common[14] : BASE;
#if PACKED
    float3 tail : TAIL;
    nointerpolation uint pair : PAIR;
#else
    float2 tail : TAIL;
    float a : HALF_A;
    float b : HALF_B;
#endif
};

[outputtopology("triangle")]
[numthreads(128, 1, 1)]
void mesh(uint i : SV_GroupIndex,
          out vertices Vertex vertices[128], out indices uint3 triangles[1]) {
    SetMeshOutputCounts(128, 1);
    float w = i == 126 ? 2.0 : i == 127 ? 4.0 : 1.0;
    float x = i == 126 ? 0.75 : -0.75;
    float y = i == 127 ? 0.75 : -0.75;
    vertices[i].pos = float4(x, y, 0.5, 1.0) * w;
    [unroll]
    for (uint row = 0; row < 14; ++row)
        vertices[i].common[row] = i + row + float4(0.25, 0.5, 0.75, 1.0);
    float a = i == 126 ? 5.0 : i == 127 ? -0.25 : -2.0;
    float b = i == 126 ? -4.0 : i == 127 ? 3.0 : 0.5;
#if PACKED
    vertices[i].tail = i + float3(0.125, 0.25, 0.5);
    vertices[i].pair = f32tof16(a) | (f32tof16(b) << 16);
#else
    vertices[i].tail = i + float2(0.125, 0.25);
    vertices[i].a = a;
    vertices[i].b = b;
#endif
    if (i == 0)
        triangles[0] = uint3(125, 126, 127);
}

#if PACKED
float2 unpack(uint pair) {
    return float2(f16tof32(pair & 0xffff), f16tof32(pair >> 16));
}

float4 pixel(nointerpolation uint pair : PAIR,
             float3 weights : SV_Barycentrics) : SV_Target {
    float2 v0 = unpack(GetAttributeAtVertex(pair, 0));
    float2 v1 = unpack(GetAttributeAtVertex(pair, 1));
    float2 v2 = unpack(GetAttributeAtVertex(pair, 2));
    return float4(mad(weights.y, v1 - v0, mad(weights.z, v2 - v0, v0)), 0, 1);
}

float4 pixel_flat(nointerpolation uint pair : PAIR) : SV_Target {
    return float4(unpack(pair), 0, 1);
}
#else
float4 pixel(float a : HALF_A, float b : HALF_B) : SV_Target {
    return float4(a, b, 0, 1);
}

float4 pixel_flat(nointerpolation float a : HALF_A,
                  nointerpolation float b : HALF_B) : SV_Target {
    return float4(a, b, 0, 1);
}
#endif
