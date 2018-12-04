﻿using System.Collections;
using System.Collections.Generic;
using UnityEngine;

public class CubeCoord
{
    public int x = 0;
    public int y = 0;
    public int z = 0;

    public CubeCoord(int x, int y, int z)
    {
        this.x = x;
        this.y = y;
        this.z = z;
    }
    public CubeCoord(int x, int y)
    {
        this.x = x;
        this.y = y;
        z = -(x + y);
    }
    public bool CheckInSameLine(CubeCoord cubeCoord)
    {
        return (x == cubeCoord.x || y == cubeCoord.y || z == cubeCoord.z);

    }
}